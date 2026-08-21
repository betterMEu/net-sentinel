# Net Sentinel

Net Sentinel 是一个使用 Rust + GPUI 构建的 Windows 本机 HTTP 黑名单拦截器。它运行一个仅监听本机的显式代理，按规则匹配请求，并把命中结果分发给可扩展 Action。

## 已实现

- HTTP：匹配 host、完整 URL、path、指定 header；支持 exact、contains、glob、regex 和 HTTP method 过滤。
- HTTPS：不安装根证书、不解密流量，仅匹配 `CONNECT` 目标域名。
- Blacklist：一条规则可绑定多个 Action。
- Action registry：内置 `popup_image`、`play_audio` 和 `serve_html`，handler 之间相互独立。
- 图片：内置拦截图，或通过界面导入 PNG/JPG/WebP/GIF/SVG。
- 音乐：3 首项目原创 WAV 旋律，或通过界面导入 WAV/MP3/FLAC/OGG。
- Windows：系统托盘、当前用户开机启动、系统代理一键接管与原设置恢复。
- 工作区：圆形水纹开关与拦截趋势位于同一概览页；规则、Actions 和设置独立展示。
- 规则编辑：在界面中新增、选择、修改、启停、删除规则，绑定多个 Action，并用 URL 即时测试命中结果；保存后会刷新既有浏览器隧道，无需重启应用。
- 统计：SQLite 持久化今日/昨日/累计、Action 执行次数、24 小时/7 天/30 天趋势、规则排行和最近拦截。
- 隐私：不保存 URL 查询参数；详细日志默认保留 7 天，聚合数据默认保留 90 天，可关闭详情、清空或导出 CSV。
- 网络：自动、直接、HTTP 上游和 SOCKS5 上游四种模式，支持 Clash/Mihomo 系统代理串联以及 TUN/WireGuard/OpenVPN 系统路由。
- 配置：首次运行在 `%APPDATA%\NetSentinel\Net Sentinel\config\config.toml` 生成默认配置。

## 运行

需要最新稳定版 Rust 和 Windows 10/11：

```powershell
cargo run --release
```

应用采用安全启动：打开后只加载界面、配置和统计，不监听端口，也不修改系统代理。点击“启动保护”后才监听 `127.0.0.1:8877`；自动模式会重新读取当前 Windows/Clash 代理，并在 GitHub HTTPS CONNECT 与本机转发验证全部通过后接管当前用户的 HTTP/HTTPS 请求。接管前的 Windows 代理设置会备份，并在停止保护、从托盘退出或关闭主窗口时恢复；右上角关闭会真正退出应用。

自动路由模式在接管前读取当前系统代理：如果 Clash/Mihomo 已开启系统代理，Net Sentinel 会把它保存为上游并组成 `应用 → Net Sentinel → Clash → Internet`；如果使用 TUN 或路由型 VPN，则上游连接自然经过当前系统路由。配置的上游端口不可达时，请求会失败并显示错误，不会静默绕过 VPN 直连。

测试默认规则：接管系统代理后访问 `http://example.com/blocked`。默认规则不会拦截普通的 `example.com` 页面。

## 配置规则

完整样例见 [`config.example.toml`](config.example.toml)。通常直接使用“规则”页和“设置”页即可；高级配置文件保存后点击“重新载入”即可生效。

`target` 可选：

- `host`：域名。HTTPS 无解密模式也可用。
- `url`：完整 HTTP URL；HTTPS 只能看到由域名组成的伪 URL。
- `path`：路径和 query，仅适用于明文 HTTP。
- `header`：请求头，可用 `header_name` 缩小范围，仅适用于明文 HTTP。

`mode` 可选 `exact`、`contains`、`glob`、`regex`。字符串匹配默认忽略大小写；正则表达式按表达式自身设置处理大小写。

## 扩展 Action

`popup_image` 由 Net Sentinel 打开独立提示窗口；`play_audio` 只在本机播放声音；`serve_html` 会把命中的明文 HTTP 响应替换成指定 HTML，因此显示在浏览器当前页面。HTTPS 只暴露 CONNECT 域名，Net Sentinel 不解密 TLS，所以可以触发独立图片窗口和声音，但不能把 HTML 安全注入加密网页，只会拒绝该 HTTPS 隧道。

在 [`src/actions.rs`](src/actions.rs) 中实现 `ActionHandler`：

1. 返回唯一 `kind`。
2. 在 `execute` 中读取 `ActionDefinition.params`。
3. 在 `ActionRegistry::standard` 中注册 handler。

匹配器与代理不需要随新 Action 修改。

## 安全边界

- 默认只监听 loopback，且不会自动接管系统代理。
- 不实施 TLS 中间人，不读取 HTTPS 内容。
- 当前代理面向 HTTP/1.x；HTTP `Upgrade` 和复杂的 chunked 上传不在初版保证范围内。
- 异常强制结束进程时 Windows 来不及执行恢复逻辑；重新打开程序后点击“恢复系统代理”即可使用备份恢复。

## 内置音乐

`tools/generate_assets.py` 通过合成波形生成三首原创短旋律：`soft-chime`、`bright-bells`、`gentle-alert`。生成结果位于 `assets/sounds/`，编译时嵌入可执行文件，不依赖外部素材授权。
