# ADR-0006：配置走两个 TOML 文件，不走环境变量

- 状态：已采纳（2026-09-01，Rust 重写时）
- 相关：`config.rs` · `config.toml` · `secrets.toml`
- 取代：Python 版的 `os.environ` + `.env`

## 决定

一次运行的全部外部输入来自**一个目录下的两个文件**：

```
config.toml    调参与端点。进 git，谁都能读
secrets.toml   密钥。0600，不进 git，加载前强制校验权限
```

目录默认是当前工作目录，第一个命令行参数可覆盖（生产传 `/etc/chat2events`）。
**读不到、字段缺、类型不对，一律直接 panic** —— 配置错误要在进程起来的第一秒暴露。

## 为什么不继续用环境变量

**1. 环境变量装不下注释，而这个项目的配置项一半价值在注释里。**

`lookback_days = 2` 这一项，光看值是个无意义的整数；它真正要说的是：

> N=2 时「周五提问、周一回复」永远拼不起来，损失系统性落在周五值班的人头上。
> 判据是「跨 2 天以上才闭合」的占比，且要按周几分别看，> 5% 立刻调 4。

`max_tokens` 要说「跟着模型变不是跟着端点变，qwen-plus 只到 32768，超了直接报 range 错」。
`reasoning_effort` 要说「`minimal` 不等于关，只有 `none` 是真关」。
这些写进 `.env` 就是一堆 `#` 注释挂在 `KEY=VALUE` 上，没有结构、没人维护，
而且 `.env` 通常不进 git —— **注释跟着不进 git，等于没写**。
`config.toml` 进 git，注释跟着配置项走。

**2. 「密钥 / 非密钥」这条线，环境变量表达不了。**

env 是一个扁平命名空间，`LLM_MODEL` 和 `LLM_API_KEY` 长得一样，
靠命名约定区分谁能进 git。分成两个文件之后这条线是**文件级**的，
`.gitignore` 里一行 `/secrets.toml` 就守住了，不依赖任何人记得。

**3. 环境变量没有权限位。**

`secrets.toml` 加载前强制 `mode & 0o077 == 0`（照 ssh 对私钥的规矩），
权限过宽直接拒绝启动并告诉你 `chmod 600`。
env 里的密钥对同机任何进程可见（`/proc/<pid>/environ`、`ps e`），
且会被子进程继承 —— 跑批要 fork 任何东西的话，密钥跟着漏出去。

**4. 类型和嵌套是免费的。**

`toml` + `serde` 直接给出 `IngestConfig` / `LlmConfig` / `MysqlConfig` 三段结构，
`u32` / `f32` / `PathBuf` / `ReasoningEffort` 全部在反序列化时就定死。
env 全是字符串，每个取值点自己 parse、自己处理 parse 失败。

## 代价，认了

- **容器化时多一步。** env 是容器的原生配置通道，改成文件就得挂 volume 或
  ConfigMap/Secret。今天跑批是定时任务不是容器服务，这个代价没发生；
  真上容器时 Secret 挂成文件也是常规做法。
- **改一个值要编辑文件，不能 `FOO=1 ./chat2events-rs`。** 唯一的例外留给了日志：
  `RUST_LOG` 存在就覆盖 `config.toml` 的 `log.level` —— 临时排障是真的高频，
  而日志级别不是承重的东西。**这个例外只此一个，不要扩散。**

## 不做

- **不做 env 覆盖层**（`CHAT2EVENTS_LLM__MODEL` 那种）。两套配置来源意味着
  「这个值到底从哪来的」需要推理，而配置排查本来就是最不该需要推理的地方。
- **不做多环境 profile**（`config.dev.toml` / `config.prod.toml`）。
  环境差异靠传不同的目录，`/etc/chat2events` 里那份就是生产那份。
- **不给 `secrets.toml` 做兜底路径搜索。** 只在给定目录里找，找不到就崩。
