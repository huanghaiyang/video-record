# ScreenRec · Windows 屏幕录制工具（Rust）

用 Rust 编写的 Windows 录屏软件：**整块屏幕录制**、**只录制某一个独立窗口**、
**多格式导出**以及**可自定义的录制文件夹**。图形界面（egui）与命令行共用同一套录制管线。

```
┌──────────────┐   ┌────────────────────┐   ┌─────────────┐   ┌──────────────┐
│ 采集源        │   │ 采集后端            │   │ 恒定帧率整形 │   │ 导出后端      │
│ 显示器 / 窗口 │──▶│ WGC / DXGI 复制     │──▶│ 补帧 / 丢帧  │──▶│ 内置 MF 编码 │
│ 系统选择器    │   │ (自动回退)          │   │ 音视频对齐   │   │ FFmpeg / GIF │
└──────────────┘   └────────────────────┘   └─────────────┘   └──────────────┘
```

---

## 1. 功能

### 录制源
- **全屏录制**：枚举全部显示器，主显示器置顶，可指定任意一块；支持 4K/HDR 显示器的
  HDR→SDR 转换（16 位浮点 → sRGB）。
- **单个窗口录制**：列出所有可捕获的顶层窗口（标题、进程、尺寸、前台标记），
  支持按标题/进程名过滤，也可直接用窗口句柄（`0x…`）。
- **区域录制**：只录制画面中的一块矩形（例如只录游戏画面、只录聊天窗口内容而不含边框）。
  坐标以物理像素计、相对所录显示器/窗口的左上角：
  - 界面上可直接填 X/Y/宽/高，或用「取第一角 → 移动鼠标 → 取对角」的方式框选
    （直接读鼠标的物理坐标，不受 DPI 缩放影响）；
  - 命令行用 `--region 100,200,800,600` 或 `--region 800x600+100+200`；
  - 区域超出画面时自动钳制到边界内，并在录制记录里写下**实际**使用的区域。
- **系统选择器**：调用 Windows 自带的「图形捕获选择器」，鼠标点选窗口或屏幕。
- 记住上次使用的录制源；目标窗口被关闭时自动结束并保存。

### 采集后端（两种，可自动回退）
| 方式 | 能力 | 说明 |
| --- | --- | --- |
| **WGC**（Windows Graphics Capture） | 整屏 + 单窗口 | 默认，性能最好；Windows 10 1903+ |
| **DXGI**（Desktop Duplication） | 仅整屏 | 当 WGC 被系统拒绝或不可用时自动接管 |

`auto`（默认）会先尝试 WGC，遇到权限/兼容问题自动切换 DXGI，并在日志中说明原因。
遇到「拒绝访问」「远程桌面」「独占全屏」等场景时依然能录。

### 导出格式
| 容器 | 编码器 | 后端 | 是否需要 FFmpeg |
| --- | --- | --- | --- |
| MP4 (`.mp4`) | H.264 / H.265 | 内置 Media Foundation | 否 |
| AVI (`.avi`) | MJPEG / MPEG-2 | 内置 | 否（取决于系统是否装了对应 MFT） |
| WMV (`.wmv`) | WMV9 | 内置 | 否 |
| MPEG-2 (`.mpg`) | MPEG-2 | 内置 | 否（取决于系统 MFT） |
| **Matroska (`.mkv`)** | H.264 / H.265 / AV1 / MPEG-4 | FFmpeg | 是 |
| **WebM (`.webm`)** | VP9 / VP8 / AV1 | FFmpeg | 是 |
| **QuickTime (`.mov`)** | H.264 / H.265 / ProRes | FFmpeg | 是 |
| **动图 GIF (`.gif`)** | 256 色调色板 | 内置（纯 Rust） | 否 |
| **PNG 序列** | 每帧一张 PNG | 内置（纯 Rust） | 否 |
| **JPEG 序列** | 每帧一张 JPEG | 内置（纯 Rust） | 否 |

- 画质：流畅 / 标准 / 高清 / 超清 / 自定义码率。
- 帧率 1–240 fps（GIF 自动限制为 15 fps、宽度上限 640）。
- 缩放：原始 / 1080p / 720p / 480p / 一半，并可再设宽度上限。
- 声音：系统声音（WASAPI 回环）、麦克风，可混音、可调音量，支持 AAC / MP3 / FLAC / PCM
  / Opus（Opus 需 FFmpeg）。
- FFmpeg 不是必需：未安装时仍可导出 MP4 / AVI / WMV / MPEG-2 / GIF / 图像序列。

### 保存与自动化
- 可自由设置**录制文件夹**（支持任意路径），界面内直接选择。
- 文件名模板：`{date}` `{time}` `{datetime}` `{source}` `{frames}`，自动去重（`_1`、`_2`…），
  非法字符与 Windows 保留名自动规避。
- 保存后自动打开文件夹（可选）。
- **全局快捷键**（窗口不在前台也生效）：开始/停止、暂停/继续、截图，可自行改键。
- 开始前倒计时（0–30 秒）。
- 暂停/继续：暂停时间不会进入视频时间轴。
- 实时预览缩略图 + 录制时长/帧数/文件大小/实际编码器。
- 截图：任意时刻抓取当前显示器一帧，保存为 PNG/JPEG/BMP。
- 录制测试图案：不采集屏幕即可验证编码设置与文件封装。

### 录制记录
每次录制完成后自动写入一条记录（`history.json`），界面「记录」页签与
`screenrec history` 命令都能查看：

- 记录内容：时间、录制源、容器/编码器、实际后端、时长、帧数与帧率、
  分辨率（源与输出）、音轨、文件大小、完整路径。
- 界面操作：双击行直接打开文件，另有「打开文件 / 在文件夹中显示 / 复制路径 /
  从记录中删除」，以及「刷新」（重新检查文件是否还在）与「清空记录」。
- 删除记录**不会删除录制文件**；文件被移动或删除后会在列表中标为「文件丢失」。
- 命令行同样可用，且支持 JSON 便于脚本处理；GUI 与 CLI 共用同一份记录，
  所以命令行录的内容也会出现在界面里。
- 记录文件与配置放在一起（见下），最多保留最近 500 条，写入采用「临时文件 + 改名」
  的原子方式，损坏时自动备份为 `history.json.broken` 并重新开始，不影响录制。

---

## 2. 环境要求

- Windows 10 1903 及以上（Windows 11 推荐），x64。
- 无需安装 .NET/运行库：产物是单个原生可执行文件。
- 可选：FFmpeg（放在 `PATH`、程序同级目录或本仓库 `.tools/ffmpeg/bin/`，也可在设置里指定路径）。

---

## 3. 构建

### 3.1 本仓库自带的离线工具链（推荐，脚本化）

本机没有全局 Rust、且 schannel TLS 不可用的环境下，仓库内置了一套工具链与
crates.io 的 HTTP 桥接，全部落在仓库内部（`.toolchain/`），不污染系统：

```powershell
# 一次性：安装仓库本地 Rust 工具链（通过国内 HTTP 镜像）
.\scripts\registry-proxy.ps1          # 另开窗口常驻：crates.io 的本地 HTTP 桥
.\scripts\cargo.ps1 build --release
```

`scripts/env.ps1` 会把 `RUSTUP_HOME`/`CARGO_HOME` 指向 `.toolchain/`，并加载 MSVC 2022
的编译环境（`vcvars64.bat`）。任何 PowerShell 会话里 `. .\scripts\env.ps1` 之后即可直接使用
`cargo`。

### 3.2 普通开发机

```powershell
cargo build --release
# 产物：
#   target\release\screenrec.exe       图形界面（GUI 子系统，双击不会弹出控制台）
#   target\release\screenrec-cli.exe  同参数的命令行版本（控制台程序，脚本请用这个）
```

图标由 `build.rs` 嵌入 exe，需要 Windows SDK 的 `rc.exe`；没有时构建照常成功，只是 exe 无文件图标。

可选：把 FFmpeg 下载到仓库内（自动被识别）：

```powershell
node scripts\fetch-ffmpeg.mjs
```

### 3.3 应用图标

图标是**用代码画出来的**，仓库里不放二进制美术资源（只有生成结果），改完可以重新生成：

```powershell
cargo run --release --example make_icon
# assets/icon.png（512x512，窗口图标）
# assets/icon.ico（16/24/32/48/64/128/256，可执行文件图标）
# assets/icon_32_preview.png（32px 预览，用于检查任务栏尺寸下是否清晰）
```

画法：每种尺寸都按 4 倍分辨率绘制再做盒平均下采样，因此边缘是抗锯齿的；
图形是圆角深色底板 + 显示器轮廓 + 红色录制圆点（16px 下只剩底板和红点，仍能辨认）。

图标有两个落点：

| 用途 | 做法 |
| --- | --- |
| 运行时的窗口/任务栏图标 | `assets/icon.png` 由 `include_bytes!` 编进程序，启动时解码成 egui 图标 |
| 资源管理器里的文件图标 | `assets/icon.rc` + `build.rs` 调用 Windows SDK 的 `rc.exe` 编译 `icon.ico`，把 `.res` 交给链接器 |

`build.rs` **直接调用 rc.exe，不引入额外依赖**；找不到 rc.exe（没装 Windows SDK）时只打印
一条警告并跳过，不会让构建失败——只是 exe 没有文件图标。

---

## 4. 使用

### 4.1 图形界面

直接运行 `screenrec.exe`（不带参数）。

- 左侧：**整块屏幕 / 单个窗口** 切换、窗口过滤、系统选择器。
- 中间「录制」页：录制控制、实时预览、导出格式、声音、保存位置。
- 「设置」页：捕获行为、采集方式、快捷键、FFmpeg、主题、配置文件位置。
- 「关于」页：版本与全部格式支持情况（按当前是否安装 FFmpeg 实时判定）。

### 4.2 命令行

程序有两个可执行文件，参数完全相同：

| 可执行文件 | 子系统 | 用途 |
| --- | --- | --- |
| `screenrec.exe` | GUI（Windows） | **双击启动图形界面，永远不会弹出控制台窗口**；带参数时也能当命令行用 |
| `screenrec-cli.exe` | 控制台 | **脚本/自动化请用这个**：shell 会等它结束，输出与退出码都能正常捕获 |

> 为什么分成两个：把界面程序链接成 GUI 子系统（这正是「不弹控制台」的前提）后，
> Windows 会让 shell **不等待**它结束，输出捕获也会变成竞态。所以命令行另出一个控制台版本，
> 两者共用同一套代码与参数。

```text
screenrec                      # 图形界面
screenrec record   [选项]      # 录制（screenrec-cli 同）
screenrec history  [选项]      # 查看/管理录制记录
screenrec sources  [--json]    # 列出显示器与窗口
screenrec formats              # 列出导出格式与后端
screenrec probe    [--strict]  # 逐个写出小文件，实测本机可用的编码器
screenrec frame    --out a.png # 抓取一帧画面
screenrec doctor               # 环境、路径、配置、FFmpeg、显示器、记录信息
```

常用录制参数：

```powershell
# 录主显示器 10 秒，720p/30fps，带系统声音
screenrec record --monitor 1 --duration 10 --fps 30 --scale 720p --system-audio

# 只录某个窗口（标题关键字或句柄）
screenrec record --window "记事本" --duration 30 --format mkv
screenrec record --window 0x105BA --duration 30 --format mp4

# 只录屏幕的一块区域（两种写法等价；坐标相对所录显示器左上角）
screenrec record --monitor 1 --region "100,200,800,600" --duration 30
screenrec record --monitor 1 --region "800x600+100+200" --duration 30
# 也可以裁一个窗口（只录窗口画面的一部分）
screenrec record --window "记事本" --region "0,0,400,300" --duration 10

# 使用系统选择器，手动停止（Ctrl+C）
screenrec record --picker --duration 0

# 动图 / 图像序列
screenrec record --monitor 1 --format gif --duration 5 --fps 15
screenrec record --monitor 1 --format png_seq --duration 3 --fps 2

# 输出到指定文件夹并自定义文件名
screenrec record --monitor 1 --output-dir "D:\录像" --filename "{date}_{time}_会议"

# 指定采集方式（WGC / DXGI / 自动）
screenrec record --monitor 1 --capture dxgi --duration 5

# 用测试图案验证编码设置（不采集屏幕）
screenrec record --test-pattern --format mp4 --duration 5 --system-audio
```

`--json` 可让 `record`/`sources`/`history` 输出 JSON，便于脚本集成。

录制记录相关参数：

```powershell
screenrec history                    # 最近 20 条
screenrec history --limit 0          # 全部
screenrec history --json             # JSON 输出
screenrec history --open 1792127920317      # 打开指定 id 的录制文件
screenrec history --remove 1792127920317    # 只删记录，不删文件
screenrec history --verify           # 重新检查文件是否还在
screenrec history --clear            # 清空记录（不删文件）
```

### 4.3 配置文件

- 默认：`%APPDATA%\ScreenRec\`，其中
  - `config.json` —— 设置；
  - `history.json` —— 录制记录（最多 500 条，可直接编辑或删除）；
  - `logs\screenrec.log` —— 运行日志。
- **便携模式**：在 `screenrec.exe` 同级放置空文件 `ScreenRec.portable`，
  上述文件改存到 `exe目录\ScreenRecData\`。
- 自动化/测试可用环境变量 `SCREENREC_DATA_DIR=<目录>` 覆盖。

---

## 5. 已知限制

- 4K 高帧率受限于 CPU 路径：没有 GPU 着色器缩放/色彩转换，也没有 D3D 表面零拷贝编码。
  4K 建议 30 fps，见「6.1 录制卡顿的排查」。

- **本机没有可用的 MPEG-2 / MJPEG MFT 时**，AVI 与 MPG 会导出失败。用
  `screenrec probe` 先实测本机支持情况；界面会据此标记不可用组合，
  `--codec` 也可改选其它编码器（例如 `--format avi --codec h264`，走 FFmpeg）。
- **HDR 显示器**：DXGI 桌面复制只会给出 16 位浮点（scRGB）画面，程序会做
  线性→sRGB 转换并裁剪到 SDR 范围，因此高光部分会被压平；这是 DXGI 不暴露
  显示器色调曲线导致的。
- **单窗口录制只能走 WGC**：DXGI 是整屏 API。若系统拒绝 WGC，单窗口录制会失败并给出提示。
- 录制期间若源画面长时间完全静止，时间轴会用「重复上一帧」补齐，保证时长正确
  （Windows 的捕获 API 在画面不变时不一定产生帧）。
- 4K/HDR 下 DXGI 采集需要逐帧做 HDR→SDR 转换，CPU 占用明显高于 WGC。

---

## 6. 性能

单帧 CPU 开销（4K，`cargo run --release --example perf -- 3840 2160 60`，本机实测）：

| 处理 | 最初 | 现在 | 做法 |
| --- | --- | --- | --- |
| HDR：Rgba16F → BGRA | 79.3 ms | **5.6 ms** | 两张查找表融合成一张 64KB 表（每通道只查一次）+ 按行并行 |
| 缩放 4K → 1080p | 119.2 ms | **4.1 ms** | 手写 BGRA 整数缩放，去掉浮点卷积与两次通道交换 + 按行并行 |
| 缩放 4K → 720p | 59.3 ms | **3.1 ms** | 同上；整数倍缩小走盒平均 |
| MF 行序翻转 | 4.8 ms | **4.7 ms** | 保持单线程：实测并行反而更慢（见下表） |
| 区域裁剪 800x600 | 0.06 ms | 0.10 ms | 逐行 memcpy，已是带宽下限 |
| 预览（4K） | 0.2 ms | 约 0 | 已按 200 ms 节流，不是热点 |

按「采集源 60 Hz」统计整条管线（编码器为空实现，只计 CPU 时间）：

| 场景 | 每源帧 | 每秒采集源 |
| --- | --- | --- |
| 4K 全屏 → 30 fps | 1.5 ms | 90 ms（约一半源帧被判定「未到时间」而完全跳过） |
| 4K 全屏 → 60 fps | 2.7 ms | 165 ms |
| 4K → 1080p，30 fps | 2.5 ms | 151 ms |
| 4K 区域 800x600 → 30 fps | 0.26 ms | 16 ms |

并行是否有用是**实测**决定的，同一二进制交替跑两轮（`SCREENREC_SINGLE_THREAD=1` 可强制单线程）：

| 处理（4K） | 单线程 | 按行并行 | 决定 |
| --- | --- | --- | --- |
| HDR 转换 | 20–24 ms | 5.1–5.6 ms | 并行（约 4 倍） |
| 缩放 → 1080p | 15–17 ms | 4.1–5.7 ms | 并行（约 3 倍） |
| 行序翻转 | 4.4 ms | 4.7–5.1 ms | **单线程**：纯 memcpy，受带宽限制，线程唤醒只添开销 |

具体改动：

- **未到时间的帧直接跳过**：原先每个采集到的帧都会先裁剪、缩放、更新预览，再由定速器决定
  是否输出 —— 60 Hz 屏幕录 30 fps 时，一半的帧做完这些工作后被整个丢掉。现在先判断
  「时间轴是否需要新帧」，不需要就直接返回。行为不变（这些帧本来就不会输出），
  但高刷新率下的开销几乎减半。
- **不再给复用缓冲区清零**：`clear()` + `resize()` 会把整块缓冲区写成 0，而它马上就会被覆盖 ——
  4K 下这是每帧几十 MB 的无用写入。改为只在长度变化时调整；这也是「翻转并行化」一开始
  反而变慢的原因（清零 + 并行开销叠加）。
- **HDR 转换**：原先每个通道要查一次 256 KB 的「半精度→f32」表，再查一次「线性→sRGB」表，
  对 256 KB 的表做随机访问（缓存不友好）。现在融合为一张 64 KB 的「半精度→u8 sRGB」表，
  每通道只查一次；超过 512 KB 的帧再按行切给多个线程。输出与旧实现逐字节一致（有等价性单测）。
- **缩放**：不再用 `image` 的浮点 Triangle 卷积（还要两次 BGRA/RGBA 全帧拷贝），
  改为直接对 BGRA 做整数运算：整数倍缩小用盒平均（面积平均，高倍缩小时不产生摩尔纹），
  非整数比用 16.16 定点双线性。两条路径都有单测，并目视核对过输出画面。
- **补帧不再多拷一份**：原先每输出一帧都要把整帧拷进「补帧缓存」，补帧时又把同一帧拷回自身
  再丢弃。现在改为**移交缓冲区**（交换所有权，零拷贝），每帧只保留一次用于补帧的拷贝。
- 并行只按**行带**切分，各线程读写互不重叠的区间，结果与线程数无关、可复现。

> 以上只统计 **CPU 侧**开销。采集本身（驱动 + WGC/DXGI 拷贝）与编码器耗时依赖真实桌面
> 会话与具体编码器，在这个环境里无法测量，因此没有列进来，也不做推测。
> 想强制单线程对比：设 `SCREENREC_SINGLE_THREAD=1`。

### 6.1 录制卡顿（像幻灯片）的排查

录制时每 2 秒会在日志里打印一行统计，先看它再谈优化：

```text
录制统计：采集 28.1 fps（57 帧，跳过 0 帧）· 输出 60.2 fps（122 帧，其中重复 65）
         · 单帧处理 18.7 ms · 编码 6.2 ms（峰值 9.5 ms）
采集跟不上目标帧率：约 53% 的输出帧是重复帧，画面会像幻灯片。
```

- **采集 N fps**：源实际送来多少帧；**跳过**是目标帧率用不到的帧（60 Hz 屏幕录 30 fps 时会跳一半）。
- **输出 N fps / 重复**：输出帧率靠**重复上一帧**撑到目标值时，就会像幻灯片。
  重复有两个来源：定速器一次要补多帧（时间轴落后），以及源停止投递时的补帧。
  **重复占比高 = 卡**，超过 1/3 会同时打一条警告。
- **单帧处理 / 编码**：单帧的 CPU 开销（单帧处理已包含编码），编码那项是专门计时出来的。

上面那段真实日志就是「4K 录 60 fps」的典型结果：采集只有 28 fps，输出 60 fps 里一半是重复帧。
**把帧率降到 30 fps 后**同样机器变成：`采集 47.9 fps（跳过 37）· 输出 29.7 fps（其中重复 0）· 单帧处理 8.9 ms`。

为什么会卡：**采集回调是同步的**。WGC / DXGI 把帧交给回调后，要等回调返回才继续投递下一帧；
如果「单帧处理 + 编码」超过了帧间隔，系统就丢帧，时间轴再用重复帧补齐 → 看起来一卡一卡。
4K 下这条链路目前每帧约 17 ms（单帧处理约 11 ms + 编码提交约 6 ms），所以：

| 场景 | 结论 |
| --- | --- |
| 4K @ 30 fps | 有余量（每帧预算 33 ms），实测输出稳定 30 fps、补帧 0–4 |
| 4K @ 60 fps | **不够**（预算 16.7 ms，实测 17 ms 左右），会大量补帧 |
| 4K HDR | 采集侧还要多一次 16 位浮点→BGRA 转换（约 5–7 ms），更紧张 |
| 缩放 1080p | 多约 2–4 ms，但编码器省得多；分享用途推荐 |

**GPU 用在哪里**：采集本身走 GPU（WGC / DXGI Desktop Duplication），但帧要读回 CPU 才能处理；
缩放、HDR/色彩转换、行序翻转都在 **CPU**（已用手写整数算法 + 行并行）；内置 Media Foundation
编码器请求了硬件加速（`SetHardwareAccelerationEnabled(true)`），FFmpeg 后端只有在打开
「优先硬件编码」时才用 NVENC/QSV/AMF。**目前没有 GPU 着色器缩放/转换路径**，这是 4K 高帧率的
真正上限所在。

**缓冲区**：`staging` / `duplicate` / `crop_scratch` / `mf_frame` 都是跨帧复用的分配，不会每帧 malloc；
但**采集与编码之间没有帧队列**，编码是同步提交的（见上）。

能做的调整，按性价比排序：

1. **降低帧率**：4K 用 30 fps；要 60 fps 就配合第 2、3 条。
2. **输出缩放到 1080p**（分享/教程够用），编码与后续处理都变小。
3. **录制时关闭显示器 HDR**：采集格式从 16 位浮点变回 8 位 BGRA，省掉整帧转换。
4. **打开「优先硬件编码」**（设置里，或配置文件 `prefer_hardware_encoder = true`），
   并改用 FFmpeg 后端（MKV/MOV/WebM）以走 NVENC/QSV/AMF。
5. 区域录制：只处理选中的那块，开销随面积下降。

已做的优化（都与上面这条链路直接相关）：HDR 转换融合查找表 + 行并行、手写整数缩放、
**行序翻转融合进已有的那次拷贝**（原来 MF 路径要多拷一次 4K 帧）、跨帧不重复清零、
未到时间的帧零开销跳过、补帧零拷贝移交。

## 7. 代码结构

```
src/
  main.rs            入口：有子命令走 CLI，否则打开图形界面
  lib.rs             库入口（GUI 与 CLI 共用）
  cli.rs             命令行：record / history / sources / formats / probe / frame / doctor
  config.rs          配置读写、便携模式、取值范围校验
  formats.rs         容器/编码器/画质能力矩阵（纯数据 + 单元测试）
  region.rs          区域录制用的矩形：解析、钳制、坐标换算
  paths.rs           输出目录、文件名模板、去重、合法化、在资源管理器中显示
  history.rs         录制记录：持久化、去重、上限、原子保存、损坏恢复
  units.rs           时长/大小/中文列宽等显示格式化
  ffmpeg.rs          查找并探测 FFmpeg
  logging.rs         文件日志（写不进去也不影响录制）
  hotkeys.rs         全局快捷键（RegisterHotKey + 独立消息循环）
  capture/
    mod.rs           采集源枚举与采集方式
    sources.rs       显示器/窗口枚举（含 GDI 桌面位置与鼠标坐标）
    pipeline.rs      帧管线：区域裁剪、编码器懒创建、恒定帧率整形、音视频对齐、预览
    handler.rs       Windows Graphics Capture 回调（薄适配层）
    dxgi.rs          DXGI 桌面复制采集循环（含 HDR→SDR）
    screenshot.rs    单帧截图
    session.rs       会话编排：校验、输出规划、后端选择、线程与状态
  encode/
    mod.rs           FrameSink 抽象与工厂
    native_video.rs  内置 Media Foundation 编码器
    ffmpeg_sink.rs   FFmpeg 管道（raw BGRA → ffmpeg，WAV + 二次封装音轨）
    gif_sink.rs      纯 Rust 动图 GIF
    image_sink.rs    纯 Rust PNG/JPEG 序列
    util.rs          像素转换、缩放、HDR 解码
  audio/
    mod.rs           音频采集与混音（按视频帧取样本，保证音画同步）
    wasapi_capture.rs WASAPI 回环 / 麦克风采集线程
    ring.rs          有界采样环形缓冲
  gui/
    mod.rs, app.rs, theme.rs, widgets.rs   图形界面（录制 / 记录 / 设置 / 关于）
build.rs             把应用图标编译进 exe（找不到 rc.exe 时降级为警告）
assets/
  icon.png           窗口图标（make_icon 生成）
  icon.ico           可执行文件图标，7 种尺寸（make_icon 生成）
  icon.rc            图标资源脚本
examples/
  dxgi_probe.rs      DXGI 像素格式/内容诊断工具
  perf.rs            逐帧 CPU 开销基准（HDR 转换、缩放、裁剪、整条管线）
  make_icon.rs       绘制并导出应用图标（含 ICO 写出与回读自检）
scripts/
  env.ps1           仓库本地 Rust 环境
  cargo.ps1         cargo 包装脚本
  registry-proxy.mjs  crates.io 的本地 HTTP 桥（受限网络/损坏 TLS 时使用）
  fetch-ffmpeg.mjs  下载静态 FFmpeg 到 .tools/
```

---

## 8. 测试

```powershell
cargo test              # 155 个单元测试
screenrec probe         # 实测本机可用的编码器（会真的写出小文件）
screenrec doctor        # 环境自检
```

**验证导出结果**（不要只看文件大小 —— 封装正确不代表画面正确）：

```powershell
# 不采集屏幕，用内置测试图案跑一遍真实编码管线
screenrec record --test-pattern --duration 2 --fps 5 --format mp4 --output-dir $env:TEMP\sr
# 抽一帧看看：图案上=横向渐变、中=纵向色阶、下=棋盘格，红条应为竖线
.\.tools\ffmpeg\bin\ffmpeg.exe -y -i "$env:TEMP\sr\*.mp4" -frames:v 1 frame.png
```

单元测试覆盖：区域解析/钳制/坐标换算/裁剪、HDR 转换融合表的等价性与边界、缩放两条路径与并行确定性、
恒定帧率整形与丢帧/补帧（含补帧内容与缓冲区别移交的回归测试）、格式能力矩阵、画质/码率换算、缩放与偶数化、文件名模板与去重、
配置迁移与钳制、恒定帧率整形与时间戳、GIF 延时进位、HDR 半精度解码与 sRGB 映射、
Media Foundation 行序（避免画面上下翻转）、音频环形缓冲与混音（增益/裁剪/静音补齐）、
FFmpeg 参数生成、WAV 头、快捷键解析、录制记录的读写/去重/上限/原子保存/损坏恢复、
中文表格宽度对齐、预览缩放、PNG/JPEG 序列写出与编码器能力探测等。

---

## 9. 许可

MIT
