# rd 官网（T-042）

单页静态站：三句话定位 + 下载 + 隐私政策（大白话版）。
纯静态无构建，`index.html` 一个文件。

## 本地预览

```bash
python -I -m http.server 8090 --directory website
# 浏览器开 http://localhost:8090
```

## 部署（GitHub Pages）

仓库远端就绪（T-039）后：

1. **替换 OWNER 占位**：`index.html` 里所有 `github.com/OWNER/rd` → 实际仓库
   （3 处：JS 下载解析、3 个下载按钮 href、页脚）。替换后 JS 会自动拉最新
   Release 资产填下载链接，失败时兜底 releases/latest 页。
2. 推送后启用 Pages（Settings → Pages → Source: GitHub Actions），
   `.github/workflows/deploy-pages.yml` 会把 `website/` 发布上去。
3. 自定义域名（可选）：CNAME 后在 Pages 设置里绑定。

## 注意

- 仓库需公开（下载链路依赖 GitHub Release）；
- GitHub 国内下载慢是已知取舍（后续可挂国内 CDN，不在本卡范围）；
- 隐私政策四类数据（审计/崩溃/信任授权/连接通道）与 10_product/商业化路线图
  三条底线、T-040 崩溃上报的设计对齐。
