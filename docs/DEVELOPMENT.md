# 开发环境说明

本仓库可以在一台**没有全局 Rust、且系统 TLS（schannel）不可用**的 Windows 机器上
从零构建。这一页记录当时用到的全部机制，便于在别的机器上复现或迁移。

## 1. 仓库本地工具链

工具链装在仓库内（`.toolchain/`，已被 `.gitignore` 忽略），不动系统环境变量：

```
.toolchain/
  rustup/                  RUSTUP_HOME
  cargo/                   CARGO_HOME（含 config.toml 与 registry 缓存）
  proxy-cache/             crates.io 镜像的本地缓存
```

安装（当时执行过的等效命令）：

```powershell
$env:RUSTUP_HOME="<repo>\.toolchain\rustup"
$env:CARGO_HOME ="<repo>\.toolchain\cargo"
$env:RUSTUP_DIST_SERVER="http://mirrors.aliyun.com/rustup"
$env:RUSTUP_UPDATE_ROOT="http://mirrors.aliyun.com/rustup/rustup"
.\rustup-init.exe -y --no-modify-path --default-host x86_64-pc-windows-msvc `
    --profile default -c rustfmt -c clippy
```

日常使用只需：

```powershell
. .\scripts\env.ps1      # 设置 RUSTUP_HOME/CARGO_HOME/PATH，并加载 MSVC 环境
cargo build --release
```

`scripts/env.ps1` 会自动查找

```
D:\Program Files\Microsoft Visual Studio\2022\{Community,Professional}\VC\Auxiliary\Build\vcvars64.bat
C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools\VC\Auxiliary\Build\vcvars64.bat
```

并解析 `set` 的输出注入 `INCLUDE`/`LIB`/`PATH`，因此链接无需手工开「开发者命令提示符」。

## 2. crates.io 的本地 HTTP 桥

**症状**：这台机器上所有走 schannel 的 HTTPS 都会失败：

```
curl: (35) schannel: AcquireCredentialsHandle failed: SEC_E_NO_CREDENTIALS (0x8009030e)
```

PowerShell/.NET 的 `Invoke-WebRequest`、系统 `curl.exe`、以及 **cargo 自带的 libcurl**
都受影响，因此 `cargo` 无法访问 crates.io。而 Node.js / Python / git（OpenSSL）与纯 HTTP
都是正常的。

**对策**：`scripts/registry-proxy.mjs` 起一个本地 HTTP 服务，代理 crates.io 的 sparse
索引与 `.crate` 下载，并把索引 `config.json` 的 `dl` 改写成指向自己：

```powershell
.\scripts\registry-proxy.ps1          # 前台常驻，默认 127.0.0.1:8799
```

`.toolchain/cargo/config.toml` 里已经写好替换源：

```toml
[source.crates-io]
replace-with = "http-mirror"
[source.http-mirror]
registry = "sparse+http://127.0.0.1:8799/index/"
```

校验和仍由 cargo 依据索引校验，桥只负责搬运字节；下载过的 crate 会缓存到
`.toolchain/proxy-cache/`，之后的构建不再需要网络。

> 在 TLS 正常的机器上，直接删除 `.toolchain/cargo/config.toml` 中的替换源即可，
> 其余代码与脚本无需改动。

## 3. FFmpeg（可选）

```powershell
node scripts\fetch-ffmpeg.mjs            # 下载静态构建到 .tools/ffmpeg/bin/
```

程序按以下顺序查找 `ffmpeg.exe`：设置里的路径 → exe 同级目录 → exe 同级 `ffmpeg/bin`、
`tools/ffmpeg/bin` → 向上 4 层目录里的 `.tools/ffmpeg/bin`、`tools/ffmpeg/bin` → `PATH`。

`scripts/fetch-ffmpeg.mjs` 的最后一步会尝试读取 `ffmpeg -version`；在禁止管道 stdio 的
沙箱里这一步会 `EPERM`，脚本已把它降级为提示，不影响安装结果。

## 4. 沙箱/受限环境下的注意事项

在这次构建环境里观察到以下限制，代码已针对它们做了处理（也顺带提升了软件健壮性）：

| 现象 | 处理方式 |
| --- | --- |
| 无法写入 `%APPDATA%` | 日志初始化失败不再中断程序；支持 `SCREENREC_DATA_DIR` 与便携模式 |
| WGC 创建捕获目标返回 `E_ACCESSDENIED` | 先转出真实 HRESULT 再报错（库本身会吞掉），并自动回退到 DXGI |
| DXGI 只给出 `R16G16B16A16_FLOAT` | 实现半精度解码 + 线性→sRGB 查找表 |
| Media Foundation 无法写入某些路径 | 报错信息直接透出，`--output-dir` 指向用户目录即可 |
| 采集源长时间无新帧 | 帧管线用「重复上一帧」补齐时间轴，`Recorder::wait_for` 设上限避免卡死 |

## 5. 验证一次录制（不要只看文件大小）

容器里出现了 `avc1` / `mp4a` 只说明**封装**正确；画面方向、时间轴、音画同步都必须解码回来看。
本仓库就因此漏掉过一个「内置 Media Foundation 编码器把画面垂直翻转」的问题：

```powershell
# 不采集屏幕，用内置测试图案跑真实编码管线
.\target\release\screenrec.exe record --test-pattern --duration 2 --fps 5 --format mp4 --output-dir $env:TEMP\sr
# 用仓库里的 ffmpeg 抽一帧
.\.tools\ffmpeg\bin\ffmpeg.exe -y -loglevel error -i "$env:TEMP\sr\*.mp4" -frames:v 1 frame.png
```

测试图案刻意做成上下不对称：**上=横向渐变、中=纵向色阶、下=棋盘格**，另有一条竖向红条。
若棋盘格出现在顶部，说明画面被垂直翻转了；红条若变成横向，说明发生了转置。

> Media Foundation 把 RGB 缓冲当作**自下而上**（除非 `MF_MT_DEFAULT_STRIDE` 为负），
> 而所用的封装库并未设置该属性，因此 `src/encode/native_video.rs` 在送入前会反转行序。
> FFmpeg 管道和内置 GIF / 图像序列写入的都是自上而下的数据，不受影响。

## 6. 常用开发命令

```powershell
. .\scripts\env.ps1
cargo check --all-targets          # 快速类型检查
cargo test                         # 单元测试
cargo clippy --all-targets         # 静态检查（工具链已装 clippy）
cargo fmt                          # 格式化
cargo run -- sources               # 查看可录制目标
cargo run -- probe --keep          # 实测编码器并保留产出文件
cargo run --example dxgi_probe 1   # 打印 DXGI 像素格式与画面统计
```
