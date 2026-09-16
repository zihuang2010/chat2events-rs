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

# 配置。跟跑批共用同一份目录，只读取 [mysql] / [log] / [web] / [roster] 四节
# （[roster] 是外部名册：Nacos 服务发现的地址、两个服务名与超时，见下面「名册配置」）
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

### 名册配置（`config.toml` 的 `[roster]` 节）

工作台要把客服的 ID 显示成姓名、把群背后的 `merchant_id` 显示成商家名称。这两份信息
不在本项目库里，属于其他业务域，服务地址由 Nacos 管理。

⚠️ **全部必填，代码里没有默认值** —— 少一个键进程起不来（跟跑批那份配置一个规矩）。

| 键 | 含义 | 示例 |
|---|---|---|
| `nacos` | Nacos 服务端地址。只要 `scheme://host:port`，**不带 `/nacos` 路径**（v1 端点由代码自己拼），不带凭证、查询串 | `http://10.0.0.9:8848` |
| `namespace` | 命名空间 ID。**用的就是 Nacos 默认值也要写出来**（默认命名空间的 ID 是空串） | `public` |
| `group_name` | 分组名。同上，默认值也要显式写 | `DEFAULT_GROUP` |
| `merchant_service` | 商家域的服务名。写错即**启动失败**，错误信息带上这个名字 | `merchant-service` |
| `employee_service` | 员工域的服务名。同上 | `employee-service` |
| `ttl_secs` | 名册（ID → 名字）整体存活多久，到期整张表清空重查。它决定「上游改名后多久在页面上看到」 | `300` |
| `timeout_secs` | Nacos 与两个业务服务共用的 HTTP 超时。内网调用，秒级即可 —— 给大了只会在上游卡住时把 `web.query_timeout_secs` 的预算一起耗掉 | `3` |

名字**只是展示**：不进任何指标、不进任何聚合键、不落库。上游查不到就回落显示 ID，
页面照常可用 —— 所以这一节配错了不会算错任何一个数字，但会让进程起不来。

### 只读账号与 Nacos 凭据

生产应给工作台配**独立的 MySQL 只读账号**，写进 `/etc/chat2events/secrets.toml` 的
`[mysql].url`。它不构造 LLM / OSS 客户端，不写表，不需要 `ingest.raw_root`。

Nacos 的账号密码在**同一个文件**的 `[roster]` 节，走**同一份 `0600` 权限检查**
（不对就直接崩，跟数据库凭据一个待遇）：

```toml
[mysql]
url = "mysql://readonly:密码@host:3306/dbname"

[roster]
username = "nacos"
password = "改成真的"
```

⚠️ 但这份 `secrets.toml` 是和跑批共用的同一个文件，里面还有 OSS 与模型密钥。
真要把权限切干净，就给工作台单独一个配置目录（只放它要的四节 + 只读账号 + Nacos 凭据），
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
journalctl -u chat2events-webui -f
# 期望三行（顺序固定）：
#   Nacos 解析到健康实例 service=merchant-service healthy=2 instances=...
#   Nacos 解析到健康实例 service=employee-service healthy=3 instances=...
#   只读工作台启动 address=127.0.0.1:8787
```

⚠️ 那两行 `Nacos 解析到健康实例` 是**「名册配对了」的唯一正向信号**，见「五、验证」第 ⓪ 条。
它们只在实例列表**发生变化**时打印（启动那次必打），之后每十秒一轮的刷新不刷屏。

停止用 SIGINT（`systemctl stop` 默认就是），会等在飞请求结束再退。

⚠️ 监听地址**保持 `127.0.0.1`**。写成 `0.0.0.0:8787` 就等于把不设防的只读接口
直接挂到公网上 —— 它没有任何鉴权，谁都能把整个库的事件摘要拉走。

## 三、配访问口令

接口没有任何鉴权，端口又在公网上，所以 nginx 这层必须挡一道。
口令直接内联在 `webui/deploy/nginx.conf` 的 `map` 里，**不用维护 htpasswd 文件**。

```bash
printf 'board:你的密码' | base64
# 把算出来的串填进 nginx.conf 的 $auth_by_pass，替换 X19SRVBMQUNFX01FX18=
```

⚠️ **必须用 `printf`，不能用 `echo`** —— `echo` 多补一个换行，算出来的 base64 对不上，
现象是口令怎么输都错，而配置看着完全正常。

加第二个人就在同一个 map 里加一行，一人一行。

原理：`auth_basic` 接受变量 —— 口令对上时它变成 `off`，认证整个跳过；对不上就去查
`/dev/null`（空文件，必然失败），返回 401 带 `WWW-Authenticate`，浏览器正常弹登录框。
本机（127.0.0.1）免密，方便下面的验证命令和探活。

### ⚠️ 改了口令的那份配置不能提交回 git

`map` 里是 base64，**不是哈希，等于明文**。仓库里那份留的是无效占位符
（谁都进不来，失败朝安全的方向倒），真口令只改目标机上的
`/etc/nginx/conf.d/chat2events-webui.conf`，两边故意不同步。

⚠️ 还有一层：HTTP 下 basic auth 的口令**明文过网**。这道闸挡的是
「扫到端口的人随手打开」，不是能抓包的对手。真要认真防就得上 HTTPS（见文末），
或者用安全组白名单。

## 四、配 nginx

```bash
cp webui/deploy/nginx.conf /etc/nginx/conf.d/chat2events-webui.conf
# 在目标机上改这两处：root 的实际路径、$auth_by_pass 的口令
vi /etc/nginx/conf.d/chat2events-webui.conf
nginx -t && systemctl reload nginx
```

`nginx -t` 报 `"map" directive is not allowed here` = map 被塞进 server 块里了，
它必须在 server 外面（conf.d 文件整个被 http 块 include，写在最外层就对）。

### SELinux（CentOS / RHEL 系）

```bash
# 不做这两步，nginx 反代会 502，日志里是 "Permission denied"
setsebool -P httpd_can_network_connect 1
semanage port -a -t http_port_t -p tcp 30001 || semanage port -m -t http_port_t -p tcp 30001
```

### 静态目录的路径权限

nginx worker 要能**穿过 root 路径上的每一级目录**（每级都要有 `x`）：

```bash
namei -om /mnt/rustserver/prod/chat2events/webui/index.html
```

输出里第一个第三组权限没有 `x` 的那一级（`drwxr-x---` 这种）就是卡点。
两种修法，挑一个：

```bash
# A. 把 nginx 用户加进属主组（目录组权限已有 x 时最干净）
NGINX_USER=$(ps -o user= -C nginx --sort=start_time | tail -1)
usermod -aG spug-deployer "$NGINX_USER"
systemctl restart nginx      # ⚠️ 必须 restart，reload 不重读补充组

# B. 逐级补搜索权限（o+x 只给「穿过」，不给列目录内容）
chmod o+x /mnt /mnt/rustserver /mnt/rustserver/prod /mnt/rustserver/prod/chat2events
```

⚠️ A 方案那个 `restart` 不能省。worker 的附加组在 fork 时就固定了，`reload` 之后
会看到**一模一样的 403**，然后以为加组没生效。

### 防火墙与安全组

```bash
firewall-cmd --permanent --add-port=30001/tcp && firewall-cmd --reload
```

阿里云还要在**安全组**里放行 30001/tcp 入方向 —— 这一步在控制台做，
机器上查不出来。8787 **不要**放行。

## 五、验证

按顺序，每一步都要过：

```bash
# ⓪ 名册配对了：两个服务各解析到几个健康实例（这是唯一的正向信号，curl 查不出来）
journalctl -u chat2events-webui | grep 'Nacos 解析到健康实例'
# 期望：两行，service= 分别是 [roster] 里那两个服务名，healthy= 都 ≥ 1
# 一行都没有 = 进程根本没起来（它启动期解析不到实例就退出），看下面的故障对照表

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

# ⑦ 外网要口令（本机免密，所以必须从外面测这条）
curl -sI http://39.98.175.5:30001/ | head -1
# 期望：401。返回 200 = auth_basic 没生效；连不上 = 安全组没放行

# ⑧ 带上口令能进
curl -sI -u board:<密码> http://39.98.175.5:30001/ | head -1
# 期望：200
```

浏览器打开 `http://39.98.175.5:30001/`，输入口令后应直接出数据。
前端的 `/api` 请求同源，浏览器会自动带上同一份凭据，不用额外处理。
**前端只用真实接口，没有 mock 兜底** —— 接口不可用时页面直接报错，
错误文案会告诉你是哪一类失败（超时 / 网络不可达 / HTTP 状态 / 数据不符合约定）。

## 六、发版更新前端

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
| 一直弹口令框，输对了也进不去 | base64 是用 `echo` 算的（多了换行），重新用 `printf` 算 |
| 外网不弹口令直接进 | `auth_basic` 写进了某个 location 而不是 server 级 |
| 409「该企业尚无已落库的群日或事件」 | `<corpid>` 填错 |
| 起不来，日志 `Nacos 登录被拒（HTTP 403）` 或 `Nacos 登录请求失败` | 前者是 `secrets.toml` 的 `[roster]` 账号密码错、或服务端压根没开鉴权；后者是 `roster.nacos` 地址不可达。先 `curl -d 'username=…&password=…' <nacos>/nacos/v1/auth/login` 手验一遍 |
| 起不来，日志 `Nacos 查不到服务 \`X\` 的健康实例` 且 X 是配的服务名 | 服务名或 `roster.namespace` 写错。在 Nacos 控制台按**命名空间**筛一遍服务列表，注意默认命名空间的 ID 是空串不是 `public` |
| 起不来，同上但服务名确认无误 | 上游根本没注册上来，或 `roster.group_name` 写错（分组不对时 Nacos 返回的是空列表，不是报错）。控制台上看那个服务的实例数与所属分组 |
| 跑着跑着日志出现 `Nacos 刷新失败，沿用上一次的实例列表` | Nacos 侧抖动。**进程不会退，页面照常**（手上那份实例列表继续用，每 10 秒重试）。持续刷就去查 Nacos 自己 |
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

## 想换成 HTTPS

basic auth 只解决「谁能进」，不解决「路上谁能看」。有域名和证书之后：
`listen 30001 ssl;` ＋ `ssl_certificate` / `ssl_certificate_key` 两行，
其余配置一个字不用改（前端走同源相对路径，协议换了自己跟着换）。

没有域名只有 IP 的话，公网 CA 签不了 IP 证书，只能自签 —— 浏览器每次红警告。
那种情况下更实际的选择是**安全组白名单**：把 30001 的入方向限制到办公出口 IP，
比任何口令都干净，且零维护。两者可以叠加。

---

⚠️ **真实 Nacos 与真实业务服务的联调是手动步骤。** 仓库里那几条名册测试打的是本地假
HTTP 服务端，只验协议形状（登录请求、令牌作查询参数、健康实例被选中、令牌过期重登），
**通过不能代替真实环境验收** —— 服务名对不对、命名空间里有没有那个服务、上游返回体
长什么样，都只有在目标机上照着上面第 ⓪ 条跑一遍才知道。

⚠️ 这份文档的步骤**没有在 39.98.175.5 上实际跑过** —— 命令是从
`src/bin/webui.rs`、`src/web/serve.rs`、`src/web/roster.rs`、`webui/src/api/client.ts` 和
`webui/deploy/nginx.conf` 读出来的。第一次照着做时把踩到的坑补回来。
