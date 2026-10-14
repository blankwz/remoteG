# clarity-bench — 文字清晰度与延迟基准工具

可重复的测试窗口（`testwin`）与像素比对工具（`compare`），用数字证明"字清晰"（逐像素一致）与端到端延迟。
对应测试策略 `09_test/测试策略.md`（文字区 PSNR = ∞ 目标）。

## 构建

```bash
cargo build -p clarity-bench --release
# 产物：target/release/clarity-bench.exe
```

仅 Windows（testwin 使用 Win32/GDI）；`compare` 纯 Rust 可跨平台。

## 子命令

### 1. testwin — 被控端测试窗口

```bash
clarity-bench testwin [--mode static|scroll|clock] [--size 1280x800]
```

- `--mode static`（默认）：静止画面，用于清晰度基准。
- `--mode scroll`：文字区每帧滚动 1 px（循环），用于运动清晰度参考。
- `--mode clock`：静止画面 + 右上角大号毫秒时间码 + 帧计数（约 60 fps），用于延迟基准。
- `--size WxH`：客户区尺寸（物理像素），默认 `1280x800`。

窗口内容（PerMonitorV2 DPI 感知，GDI 绘制，各 DPI 下按实际 DPI 缩放）：

| 区域 | 内容 | 用途 |
|---|---|---|
| 四角 | 16×16 纯 `#FF00FF` 块 + 相邻 16×16 黑白棋盘 | compare 定位标记（对齐/裁剪） |
| 左侧 | 9/10/11/12/14/16 pt，ClearType 与灰度各一行，中英文混排 | 文字清晰度 |
| 左下 | 彩色字配彩色背景：红/蓝、蓝/绿、绿/红 | 彩色文字与色彩还原 |
| 右侧 | 1 px 细线（间距 1/2/3）与 1 px 网格 | 细线/高频细节 |
| 右上 | clock 模式：`HH:MM:SS.mmm frame=N`（等宽字体右对齐） | 延迟测量 |
| 底部 | `clicks=<n> last=<x>,<y>`（同时写入窗口标题） | T-025 输入测试 |
| 右键 | 3 项上下文菜单（TrackPopupMenu） | T-025 窗口组用例 |

左键点击会更新标题：`clarity-bench clicks=<n> last=<x>,<y>`，供 T-025 通过窗口标题读取。

### 2. compare — 两张截图比对

```bash
clarity-bench compare <source.png> <received.png> [--json] [--text-region x,y,w,h]
```

流程：分别在两图中找 `#FF00FF` 像素的外接矩形 → 两侧矩形尺寸一致即按标记对齐（天然支持整图平移）→ 在对齐区域内计算指标。标记缺失或尺寸不一致时退化为整图同坐标比对（`aligned=false`）。

输出字段：

| 字段 | 含义 |
|---|---|
| `exact_ratio` | 比对区域内逐像素完全一致比例（0.0~1.0） |
| `text_exact_ratio` | 文字区一致比例。默认文字区 = 比对区域向内收缩 5%（≤24px，避开角标与棋盘）；`--text-region x,y,w,h` 可显式指定（source 坐标） |
| `psnr` | PSNR（dB）；**JSON 中 `null` 表示逐像素一致（PSNR = ∞）** |
| `max_err` | 最大单通道误差（0~255） |
| `aligned` / `region` / `text_region` | 对齐方式与实际比对区域（诊断用） |

判读标准（静止画面，09_test/测试策略.md）：

- 完美：`exact_ratio = 1.0`、`text_exact_ratio = 1.0`、`psnr = null`、`max_err = 0`；
- 可接受（有损链路）：以 `text_exact_ratio` 与 `psnr` 为主要指标，按 Epic 成功指标评估。

### 3. capture — 源窗口抓图（当前为桩）

```bash
clarity-bench capture <窗口标题> <out.png>
```

依赖 `rd-capture-win`（任务 T-002）。当前打印"未实现"并返回非零码；T-002 完成后在 `src/main.rs::run_capture` 一处接入，CLI 形态不变。

## 清晰度测量步骤（完整）

1. **被控端打开测试窗口**：`clarity-bench testwin --mode static --size 1280x800`，窗口置前且不被遮挡。
2. **截取源图 source.png**：
   - T-002 完成后：`clarity-bench capture "clarity-bench" source.png`（自动抓客户区，推荐）；
   - 之前可用任意截图工具截取测试窗口**客户区**（不含标题栏/边框），或对全屏截图——compare 会按四角品红标记自动定位客户区。
3. **控制端导出画面**：控制端连接该窗口后按 `Ctrl+Shift+S` 导出当前画面为 received.png（静止 ≥300 ms 后导出，符合测试策略"静止 300 ms 后截取"）。
4. **比对**：
   ```bash
   clarity-bench compare source.png received.png --json
   ```
5. **判读**：`exact_ratio=1.0、text_exact_ratio=1.0、psnr=null、max_err=0` 即"字清晰"达成；否则按 `max_err` 与区域分布定位劣化来源（缩放、色彩转换、编码损失）。

注意事项：
- 截图不得经过缩放；两侧须处于同一 DPI 环境（窗口为 PerMonitorV2，150% 显示下客户区即 1280×800 物理像素）。
- 控制端导出若为有损编码（JPEG 等），`psnr` 不可能为 `null`，属预期；判定以方案约定的阈值为准。
- 轻微模糊/有损会侵蚀品红标记边缘导致两侧标记外接矩形不再精确等大，此时 compare 退化为整图同坐标比对（`aligned=false`），指标仍然有效；若整图有平移则需先按标记对齐（保持无损路径）。

## 延迟测量方法（clock 模式）

### 方法 A：截图时间码比对（两端 NTP 同步）

1. 两端时钟经 NTP 同步（偏差记为 ε）。
2. 被控端：`clarity-bench testwin --mode clock`。
3. 控制端：记录按下截图的本地时刻 `t_local`（精确到 ms），截取控制端画面。
4. 读出截图中时间码 `t_remote`（`HH:MM:SS.mmm`）。
5. 端到端延迟 ≈ `t_local - t_remote`（含截图自身耗时需扣除；重复 10+ 次取中位数，报告中同时给出 ε）。

### 方法 B：输入→画面回环（无需时钟同步）

1. 被控端 `testwin --mode clock`；控制端画面与被控端同屏可见（或录屏）。
2. 在控制端触发一次可观察的输入（如点击测试窗口——底部 `clicks` 计数 +1）。
3. 用高帧率录屏（≥60 fps）或逐帧导出，测量"输入动作帧 → 画面出现变化帧"的间隔。
4. 延迟 = 帧间隔 / 帧率。重复多次取中位数。

### 方法 C：帧计数验证刷新率

clock 模式右上角 `frame=N` 每帧递增；连续两次截屏差值 ÷ 时间差即为链路实际刷新率参考。

## 开发

```bash
cargo test -p clarity-bench      # 集成测试（tests/compare.rs）
cargo clippy -p clarity-bench -- -D warnings
```

`tests/compare.rs` 覆盖任务卡三用例：同图 100%、3×3 均值模糊显著下降、平移 3 px 对齐成功，另含 PNG 往返一致性。
