# Web 字体

Aporto 将英文字体 **Google Sans Flex** 和中文字体 **Noto Sans SC** 随 Web 构建产物一起部署。浏览器从本站读取 WOFF2 文件，无需访问 Google Fonts、Fontsource CDN 或其他外部字体服务。

[README 配图](visuals/README.md)直接复用 Client UI 的 [fonts.css](../apps/web/src/fonts.css) 和 [theme.css](../apps/web/src/theme.css)，统一字体、深色配色和品牌渐变。从本地 HTML/CSS 导出 PNG 前会等待字体加载完成，不请求外部字体服务。

## 来源与许可

| 字体             | 锁定依赖                                      | 上游字体版本 | 许可文件                                                                 |
| ---------------- | --------------------------------------------- | ------------ | ------------------------------------------------------------------------ |
| Google Sans Flex | `@fontsource-variable/google-sans-flex@5.3.1` | `v22`        | [google-sans-flex.txt](../apps/web/public/licenses/google-sans-flex.txt) |
| Noto Sans SC     | `@fontsource-variable/noto-sans-sc@5.3.0`     | `v40`        | [noto-sans-sc.txt](../apps/web/public/licenses/noto-sans-sc.txt)         |

以上字体均为 SIL Open Font License 1.1（`OFL-1.1`）。已核验 Fontsource 包内的 `metadata.json`、`LICENSE`，以及 Google Fonts 官方目录中的元数据与 OFL 文件：

- Google Sans Flex：[官方目录](https://github.com/google/fonts/tree/main/ofl/googlesansflex)、[字体项目](https://github.com/googlefonts/googlesans-flex)、[Fontsource 包](https://www.npmjs.com/package/@fontsource-variable/google-sans-flex)。
- Noto Sans SC：[官方目录](https://github.com/google/fonts/tree/main/ofl/notosanssc)、[字体项目](https://github.com/notofonts/noto-cjk)、[Fontsource 包](https://www.npmjs.com/package/@fontsource-variable/noto-sans-sc)。

字体文件未修改；`package-lock.json` 固定版本及完整性校验值。包中的版权声明和完整许可原样保存在 `public/licenses/`，Vite 会将它们复制到构建产物的 `/licenses/`。分发 `dist/` 时应保留这些文件。字体遵循 OFL，允许随软件一起分发。

## 引入方式

`src/main.tsx` 在应用样式前引入 `src/fonts.css`；`src/styles.css` 引入共享的 `src/theme.css`。主题中的字体栈以以下字体开头，随后保留系统中文和 sans-serif 回退：

```css
font-family: "Google Sans Flex Variable", "Noto Sans SC Variable", sans-serif;
```

`fonts.css` 为 Google Sans Flex 声明 Latin、Latin Extended、Vietnamese 三个 Unicode 分片，并导入 Noto Sans SC 的完整 `wght.css`。两个字体都使用 `font-display: swap`，网络下载期间可以先显示系统字体。

Google Sans Flex 使用仅包含 `wght` 轴的资源，不引入 `full.css`，因此不会打包额外的宽度、倾斜、光学尺寸、圆角和粗细增量轴。常用的 400–700 字重共享同一字体文件，无需为每个字重下载独立文件。Google Sans Flex 的可用字重范围为 1–1000，Noto Sans SC 为 100–900。

## 中文覆盖与资源大小

Noto Sans SC 保留上游包的全部 101 个 Unicode 分片，不依据静态 UI 文案裁剪字集。后续模型输出出现新汉字时，浏览器会按 `unicode-range` 下载对应分片；不需要重新构建 UI。上游字体未覆盖的字符仍通过系统字体回退，并非声称一个字体覆盖 Unicode 中的所有 CJK 扩展字符。

以下为锁定版本中实际引用的 WOFF2 原始文件大小，不含 CSS、许可文件或 HTTP 压缩：

| 资源                                                    | 文件数 |                     总大小 |
| ------------------------------------------------------- | -----: | -------------------------: |
| Google Sans Flex（Latin / Latin Extended / Vietnamese） |      3 |    87,528 字节（85.5 KiB） |
| Noto Sans SC（完整 Unicode 分片）                       |    101 | 4,516,508 字节（4.31 MiB） |
| 合计                                                    |    104 | 4,604,036 字节（4.39 MiB） |

这些文件随 `dist/` 一起分发，但浏览器仅请求当前文本实际需要的分片。常规英文会使用其中 50,832 字节的 Google Sans Flex Latin 文件；中文下载量取决于当前页面和会话中的字符。不要将所有中文分片添加到 preload。

## 离线部署

依赖获取阶段执行 `npm ci`；之后执行常规 `npm run build`，Vite 会解析 npm 包中的字体引用，并生成带内容哈希的本地资源。部署整个 `dist/`，包括 `assets/` 和 `licenses/`。已经构建的静态站点不需要字体 CDN 的网络访问；离线构建则需要预先准备 npm 缓存或依赖镜像。

验证字体加载时，可在浏览器网络面板筛选 `woff2`，确认请求来自当前 UI origin；再查看实际渲染字体，确认拉丁字符为 Google Sans Flex、汉字为 Noto Sans SC。中文新字符应触发本站对应 Unicode 分片，而非外部请求。
