# rd 虚拟屏驱动包 —— 归属与来源标记

本目录是 rd 便携包里的内置虚拟显示器驱动（"驱动三件套"：驱动本体 +
`install-driver.cmd` + 归属标记）。分发与卸载规则都以本文件为准。

## 内容物

| 文件 | 来源 | 许可 |
| :--- | :--- | :--- |
| `VirtualDisplayDriver.dll` / `VirtualDisplayDriver.inf` / `virtualdisplaydriver.cat` | [MolotovCherry/virtual-display-rs](https://github.com/MolotovCherry/virtual-display-rs) master `22fcd2e`，本地 WDK 10.0.19041 构建，DriverVer `0.4.0.0`（stampinf `-u 2.15.0` 展开） | MIT，见 `LICENSE-virtual-display-rs.txt` |
| `DriverCertificateMaster.cer` | rd 开发期自签代码签名证书的**公钥**部分（`CN=DriverCertificateMaster`） | rd 自制；**私钥不进仓库** |
| `nefconw.exe` v1.10.0 | [nefarius/nefcon](https://github.com/nefarius/nefcon)（创建 root 枚举设备节点 + 装 INF 的唯一轻量工具） | MIT，见 `LICENSE-nefcon.txt` |
| `install-driver.cmd` | rd 自制，一次 UAC；纯 ASCII（GBK 控制台下中文脚本会静默失败） | rd |

## 安装语义

1. 证书装入 `root` 与 `TrustedPublisher` 两个 store（只装公钥；用户机器永远
   不需要私钥，也不需要 `bcdedit testsigning`）；
2. `nefconw --remove-device-node` + `--create-device-node --class-name Display`
   （`--class-name` 不可省）+ `--install-driver --inf-path`；
3. 写归属标记 `%ProgramData%\rd\driver-ownership.json`（owner=rd、hardware_id、
   inf、provider、driver_version、installed_at）。

**覆盖升级**：hardware id `Root\VirtualDisplayDriver` 是上游固定值，官方
v0.3.1、SnugWin 等安装的是同一个节点。rd 安装包会把它升级成 rd 的 master
构建（协议是 v0.3.1 的超集，第三方控制软件仍可继续使用）；脚本检测到已存在
节点时会明确提示。

**卸载规则（未来 rd 卸载器实现依据）**：只有 `driver-ownership.json` 存在
且 `owner=rd` 时才允许 `nefconw --remove-device-node` 并删除该 JSON；否则
视为用户自装驱动，rd 卸载**不得**触碰。这条规则是本目录存在的核心原因。

## 开发期签名说明（分发前必改）

当前 DLL/cat 由开发机自签证书（`master.pfx`，密码仅本机测试用，禁止入库）。
任何人都能用同名自签证书签出同样的文件——因此**正式发布必须换正式证书重
签**（打包流水线里替换 `.cer` + 重签 `.cat`/`.dll`），并同步更新本文件与
`driver-ownership.json` 中的 provider 信息。Win11 ARM 等平台可能拒绝非
WHQL 签名的显示驱动（上游 issue #465），x64 主流场景不受影响。

## 相关

- 设计依据：`03_architecture/虚拟屏权限实测.md`（§8 master 补测）与
  `03_architecture/虚拟屏与窗口级远程预研报告.md`（§1.3 snug-win 分发实证）；
- 运行时控制面：`crates/rd-vdisplay`（master Notify/State 协议客户端）。
