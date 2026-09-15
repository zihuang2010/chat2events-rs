# 只读工作台部署（39.98.175.5:30001）

这台机器的 runbook。通用的编译、配置文件格式、目标机约束见 `docs/deploy.md`，
这里只写**和这套地址绑定的部分**，不复制那边已有的内容。

## 结论先行

| 问题 | 答案 |
|---|---|
| 前端接口地址怎么配？ | **不用配，也配不了。** `API_BASE` 写死 `/api`（`webui/src/api/client.ts:31`），按 `location.origin` 拼同源 URL |
| 那前端怎么找到后端？ | 靠 nginx：同一个 `:30001` 下，`/` 给静态文件，`/api/` 反代到 `127.0.0.1:8787` |
| 前端要重新编译吗？ | **不要。** 30001 是独立端口的**根路径**，`VITE_BASE` 默认 `/` 就对，Release 的 tar 包直接用 |
| 后端要对外监听吗？ | **不要。** 只监听 `127.0.0.1:8787`，外网只开 30001 一个口 |

访问地址：**`http://39.98.175.5:30001/`**

## 为什么必须同源反代

两条硬约束，都不是选择题：

1. **前端的 `/api` 是常量**，不读任何环境变量。`get()` 里是
   `new URL(API_BASE + path, location.origin)` —— 用户从 `:30001` 打开页面，
   请求就发到 `http://39.98.175.5:30001/api/...`，别处去不了。
2. **后端没有 CORS**（`src/web/serve.rs` 里没有任何 `Access-Control-*`）。
   就算把 `API_BASE` 改成 `http://39.98.175.5:8787`，浏览器也会在预检这一步
   直接拦掉。

所以拓扑只有一种：nginx 在 30001 上同时提供静态文件和 `/api` 反代，
后端缩在 `127.0.0.1:8787` 后面。

```text
浏览器 ──→ 39.98.175.5:30001 ──┬─ /        → /srv/chat2events-webui（静态）
          （nginx）            └─ /api/*   → 127.0.0.1:8787（webui 进程）→ MySQL
```

## 一、准备文件

```bash
# 二进制（Release 的 chat2events-rs-linux-x86_64.tar.gz 里）
install -m 755 webui /opt/chat2events/webui

# 配置。跟跑批共用同一份目录，只读取 [mysql] / [log] / [web] 三节
ls /etc/chat2events/          # config.toml + secrets.toml（0600，不对就直接崩）

# 前端静态站。压缩包根上就是 index.html 和 assets/，不套一层 dist/
mkdir -p /srv/chat2events-webui
tar -xzf chat2events-webui-dist.tar.gz -C /srv/chat2events-webui
ls /srv/chat2events-webui/    # 期望看到 index.html 和 assets/
```

⚠️ **别在这台机器上编前端** —— 目标机没有 Node。包由 CI 的 webui job 随 Release 发出。

### `<corpid>` 从哪来

必填位置参数，**每条查询都按它过滤**，填错不会静默返回空数据，`/api/meta` 直接 409：

```sql
SELECT DISTINCT corpid FROM b_merchant_group_event;
```

### 只读账号

生产应给工作台配**独立的 MySQL 只读账号**，写进 `/etc/chat2events/secrets.toml` 的
`[mysql].url`。它不构造 LLM / OSS 客户端，不写表，不需要 `ingest.raw_root`。

⚠️ 但这份 `secrets.toml` 是和跑批共用的同一个文件，里面还有 OSS 与模型密钥。
真要把权限切干净，就给工作台单独一个配置目录（只放它要的三节 + 只读账号），
启动时指过去。

## 二、启动后端

```ini
# /etc/systemd/system/chat2events-webui.service
[Unit]
Description=chat2events read-only webui
After=network-online.target

[Service]
Type=simple
User=chat2events
# 参数：<配置目录> <corpid> [监听地址]
ExecStart=/opt/chat2events/webui /etc/chat2events <corpid> 127.0.0.1:8787
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=multi-user.target
```

```bash
systemctl enable --now chat2events-webui
journalctl -u chat2events-webui -f     # 期望第一行：只读工作台启动 address=127.0.0.1:8787
```

停止用 SIGINT（`systemctl stop` 默认就是），会等在飞请求结束再退。

⚠️ 监听地址**保持 `127.0.0.1`**。写成 `0.0.0.0:8787` 就等于把不设防的只读接口
直接挂到公网上 —— 它没有任何鉴权，谁都能把整个库的事件摘要拉走。

## 三、配 nginx

```bash
cp webui/deploy/nginx.conf /etc/nginx/conf.d/chat2events-webui.conf
nginx -t && systemctl reload nginx
```

那份配置里四条注释都是踩过的坑，别删。最容易中招的是 `proxy_pass` 末尾的斜杠：
后端路由自带 `/api` 前缀（`/api/meta`、`/api/summary`…），写成
`proxy_pass http://127.0.0.1:8787/;` 会把前缀剥掉，全部 404。

### SELinux（CentOS / RHEL 系）

```bash
# 不做这两步，nginx 反代会 502，日志里是 "Permission denied"
setsebool -P httpd_can_network_connect 1
semanage port -a -t http_port_t -p tcp 30001 || semanage port -m -t http_port_t -p tcp 30001
```

### 防火墙与安全组

```bash
firewall-cmd --permanent --add-port=30001/tcp && firewall-cmd --reload
```

阿里云还要在**安全组**里放行 30001/tcp 入方向 —— 这一步在控制台做，
机器上查不出来。8787 **不要**放行。

## 四、验证

按顺序，每一步都要过：

```bash
# ① 后端自己活着（本机直连，绕开 nginx）
curl -s http://127.0.0.1:8787/api/meta | head -c 200
# 期望：JSON。409「该企业尚无已落库的群日或事件」= corpid 填错了

# ② nginx 反代通了，且 /api 没被静态站吃掉
curl -s http://127.0.0.1:30001/api/meta | head -c 200
# 期望：和 ① 一样的 JSON。返回 HTML = proxy_pass 配错或 location 顺序不对

# ③ 静态站在
curl -sI http://127.0.0.1:30001/ | head -3
# 期望：200，content-type: text/html

# ④ 深链接刷新不 404（try_files 生效）
curl -sI http://127.0.0.1:30001/rooms | head -1
# 期望：200，不是 404

# ⑤ /api 压缩生效（gzip_proxied any）
curl -s -H 'Accept-Encoding: gzip' -D- -o /dev/null http://127.0.0.1:30001/api/meta | grep -i content-encoding
# 期望：content-encoding: gzip  —— 没有这一行就是 gzip_proxied 没生效

# ⑥ 写操作被堵死
curl -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:30001/api/meta
# 期望：403

# ⑦ 外网真能打开
curl -sI http://39.98.175.5:30001/ | head -1
# 不通就是安全组没放行
```

浏览器打开 `http://39.98.175.5:30001/`，页面应直接出数据。
**前端只用真实接口，没有 mock 兜底** —— 接口不可用时页面直接报错，
错误文案会告诉你是哪一类失败（超时 / 网络不可达 / HTTP 状态 / 数据不符合约定）。

## 五、发版更新前端

```bash
tar -xzf chat2events-webui-dist.tar.gz -C /srv/chat2events-webui
```

不用 reload nginx，不用重启后端。`index.html` 是 `no-store`、静态资源带 hash，
用户刷新即拿到新版。

## 常见故障对照

| 现象 | 多半是 |
|---|---|
| 页面打不开，连接被拒 | 安全组没放行 30001，或 nginx 没起 |
| 页面出来了，面板全报「接口返回 404」 | `proxy_pass` 末尾多了斜杠，`/api` 前缀被剥掉 |
| 报「接口返回的数据不符合约定」，detail 提到检查反向代理 | `/api` 被 `location /` 的 try_files 接走，返回了 index.html |
| 502 | 后端没起，或 SELinux 拦了 nginx 出站连接 |
| 409「该企业尚无已落库的群日或事件」 | `<corpid>` 填错 |
| 刷新子页面 404 | `try_files` 没配 |
| 多人同时用就有面板 503 | `web.concurrency` 不够：一次开页并发 7 个请求，名额要 ≥ 在线人数 × 7 |
| 首屏慢、`journalctl` 里一堆 SLOW_REQUEST | 见 `docs/deploy.md`「只读工作台响应缓存」的索引升级 |

## 如果以后要走子路径

比如挂到 `http://39.98.175.5/board/`，那就**必须自己重编前端**，
Release 里那个包用不了（它是根路径构建）：

```bash
VITE_BASE=/board/ pnpm build
```

nginx 侧同时要改三处：`location /board/`、`try_files` 回落到 `/board/index.html`、
`/assets/` 前缀变成 `/board/assets/`。而 `/api` **始终在站点根路径**，不随
`VITE_BASE` 走 —— 前端那个常量是 `/api`，不带 base 前缀。

当前用独立端口 30001 就是为了避开这一整套麻烦。

---

⚠️ 这份文档的步骤**没有在 39.98.175.5 上实际跑过** —— 命令是从
`src/bin/webui.rs`、`src/web/serve.rs`、`webui/src/api/client.ts` 和
`webui/deploy/nginx.conf` 读出来的。第一次照着做时把踩到的坑补回来。
