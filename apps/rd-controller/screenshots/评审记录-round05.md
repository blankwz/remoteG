# Round 5 评审记录（10 视角合并评审团 · 评审对象 round04-context）

## 评分（10 人均分 69.5）

视觉终审 75 / 微文案 70 / 交互完整性 65 / 信任安全 65 / 效率 75 / 学习曲线 70 / 容错 65 / 视觉节奏 75 / 完成度 65 / 总体印象 70

## 核心判词：「修复做了但没露出来」

两位评委独立证伪 R4 的两项修复（等待占位、健康胶囊常显）。追查结果：

### 等待占位不可见 → 三层根因（本次最大收获）
1. **decoder error 路径假 stats**（R4 已修）：错误时把 canvas 默认 300×150 当分辨率上报
2. **decoder configure throw 崩 React effect**（本轮新发现）：headless/无硬解环境 configure 抛异常 → 挂载 effect 崩溃 → **整页布局随机塌陷**（stage 塌成 2px）。这是影响真实老机器用户的健壮性 bug。修复：configure try-catch + WindowSurface 构造兜底，decoder 失败保持 null 帧安全忽略
3. **headless paint 丢弃**：absolute 定位的占位在 DOM/布局/computed style 全部正常的情况下不被 headless screenshot 绘制（真实浏览器正常）。改 fixed 定位绕过——产品语义不变（占位即全屏居中提示）

### R5 修复对照
| # | 问题 | 修复 | 状态 |
|---|---|---|---|
| 1 | 占位不可见（6 位评委） | 三层根因全修 + 卡片式醒目占位 | ✅ 截图实证 |
| 2 | 健康胶囊终端会话消失 | 终端标签显示「已连接」胶囊 | ✅ |
| 3 | 锁屏/安全屏无确认 | 安全屏补 3s 二次确认（锁屏 R3 已有） | ✅ |
| 4 | 设备搜索不可见 | 显示条件 >3 改 ≥3 | ✅ |
| 5 | 最小化徽标双令牌 | 侧栏统一灰胶囊，观察中保留激活色 | ✅ |
| 6 | 「本机被控」费解 | 补副标题「这台电脑也可被其他设备远程控制」 | ✅ |
| 7 | 生成配对码不显眼 | 主色描边提级 | ✅ |
| 8 | 快捷键无可发现性 | Ctrl+Shift+S/L/V/H 标注进 tooltip | ✅ |
| 9 | 黑屏+60fps 矛盾 | 健康胶囊只在有视频标签时显示数字 | ✅ |

## 转后续轮次
- 演示模式（遮蔽 IP/标题）
- 窗口缩略图（后端）
- 工作区恢复 lastWorkspace
- tab 下划线滑动动画
- 「切换」类悬空文案巡检

## 工程收获
- 截图工具链升级：chrome --screenshot(virtual-time) → puppeteer-core + 真实等待 + 布局稳定轮询 + vite preview 生产构建
- 排障方法论：DOM→布局→绘制三层分离验证（getBoundingClientRect / getComputedStyle / 截图像素各查一层）
