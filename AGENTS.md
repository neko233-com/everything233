# AGENTS.md — everything233

Agent 操作本仓库时遵守本文件。人读 README.md，机器读这里。

## 项目定位

voidtools Everything 的 Windows 替代。单文件静态 everything.exe，只搜文件名、不读内容。
仓库名 everything233 的 233 只是个人标识，产出 exe 必须叫 everything.exe，不要改成 everything233.exe。

## 硬性约束

- Windows-only；静态 CRT（-C target-feature=+crt-static），Windows 无 musl，crt-static 即等价物。
- 0 抽象：不引框架、不写 trait 层；能用 Windows API / 系统自带工具（curl、msiexec、taskkill）就不写依赖。
- 不读文件内容；白名单制（见下「扫描范围」），绝不整盘；跳过清单：.git node_modules target Library Temp obj __pycache__ .gradle .nuget .cache，额外读 %APPDATA%\everything233\ignore.txt。
- 非提权运行；USN/MFT 直读、裸卷访问属 roadmap，不做。

## 构建环境（每开新 shell 必设）

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
$env:LIB = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207\lib\x64'
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release
```

VS18 Community 缺 CRT 导入库，只借 BuildTools 的 libcmt/libucrt，link.exe 仍用 VS18。

## 代码结构

- src/main.rs — CLI 分发：serve / install / uninstall / update / query；detach_self 后台分离。
- src/index.rs — arena 紧凑索引（父子链+名字，命中回溯拼路径）。Shared 无锁：`AtomicPtr<Index>` 发布快照，查询 Acquire 读零锁，重建 `swap` 后 drop 旧代，不用 Mutex/RwLock。build 降线程优先级(BELOW_NORMAL)且每 ~4k 项 yield_now 让出；后台 30s 仅根顶层签名比对，变化才低优先级重建，无常驻变更句柄。
- 单例：serve 开头 CreateMutexW(`everything233-single`)，`ERROR_ALREADY_EXISTS(183)` 即退出，杜绝双 daemon 翻倍 RSS。
- src/daemon.rs — 命名管道 \\.\pipe\everything233，请求 query\nlimit\nflags，UTF-16 收发。
- src/reg.rs — HKCU Run 自注册（无需管理员）。
- src/ignore.rs — 内置跳过清单 + ignore.txt。
- src/update.rs — 手动/自动更新 + 最快镜像路由。

## 扫描范围（改动前必读）

- 白名单制。`scan_roots()`：`%APPDATA%\everything233\include.txt` 逐行为扫描根（# 注释），支持盘符根（`D:\`）；缺省只扫 C: 用户办公目录（Desktop/Documents/Downloads/OneDrive/Favorites/Pictures/Videos/Music/source/scripts/DoubaoWork/WPS Cloud）。盘符根第一层由精确 TOP_SKIP 挡游戏库/缓存/临时/备份（勿用裸 game/games，会误伤 GameApp 等项目目录）。
- C: 系统树、其他盘、游戏库/缓存/临时/备份一律不进索引；数据盘不再整盘遍历。
- 忽略分两层：目录名 BUILTIN（ignore.rs）+ 完整路径 PREFIX（`\windows\` `\program files\` `\programdata\` `\appdata\` `\go\pkg\mod` `\.cargo\registry\` `\cargo\registry` `\node_modules`）。新增系统级缓存走 PREFIX，别往 BUILTIN 塞全局名。

## 内存布局与索引格式（改动前必读）

- Rec 20B：parent u32（bit31=DIR_BIT 目录标记，低31位=父id，ROOT=0x7FFF_FFFF 为盘符根）、name_id u32、size u64、mtime u32（unix 秒，非 FILETIME）。无 nlen。
- 名字去重：build 期 seen HashMap 分配 name_id 后 clear+shrink 释放；常驻只留 len(u16)-prefix 紧凑 names blob + name_off 偏移表，重复文件名不重复存字节。
- 构建期不存完整路径；命中才沿 parent 链 resolve。
- 缓存格式 v3：magic `E233` + version=3 + names blob + name_off 表 + recs（parent/name_id/size/mtime）+ watch_path（空表）。版本不符 load 返回 None，自动重建。
- 遍历死循环红线：内层 FindNextFileW 循环里禁止用 `continue` 跳过该项（会跳过 FindNextFileW 导致同项死循环），用 if 包裹。

## 更新机制（改动此处前必读）

### 手动 everything update
1. GET https://api.github.com/repos/neko233-com/everything233/releases/latest 取 tag_name，semver 比大小。
2. 最快路由：对各镜像发 HEAD 计时，选延迟最低者；失败按 MIRRORS 顺序兜底。
   - https://github.com（直连）
   - https://ghfast.top/https://github.com
   - https://gh-proxy.com/https://github.com
   - https://ghproxy.net/https://github.com
3. 下载 releases/download/<tag>/everything233.msi（内置 curl.exe），存 %LOCALAPPDATA%\everything233\update\。
4. 停本 daemon（taskkill 同名 PID，排除自身）→ ShellExecuteW runas 提权 msiexec /qn /norestart → 8 秒后后台重启 serve --background。

### 自动 everything serve --auto-update
启动 8 秒后后台线程检查；有新版先下 MSI 到 update\latest.msi；带 --auto-update 则提权安装并重启，否则只在 stderr 提示 run everything update。

### 路由原则
- 镜像只是 GitHub 前缀代理，不换下载源内容；校验以 GitHub Release 资产为准。
- 新增/淘汰镜像只改 update.rs 的 MIRRORS，不要在别处硬编码。
- API 走直连 api.github.com；被墙时手动更新直接报错，不自动切 API 代理。

## 打包与发版

- MSI：WiX v7，wix build packaging\Product.wxs -o dist\everything233.msi -arch x64（cwd=仓库根）。
- 资产：everything233.msi + everything233-windows-x64.zip（内含 everything.exe）。
- 版本号改 Cargo.toml，同步 packaging\Product.wxs 的 Version。
- 用户说「发布」= 自动 cargo build → 打 MSI/zip → gh release create vX.Y.Z 传资产，无需再确认。
- install.ps1 从 latest release 拉 MSI，UAC 装，写用户 PATH，装完自注册并起 daemon。

## 禁止

- 删个人目录文件；碰回收站/Windows.old/还原点需用户单项确认。
- 改 System32/WinSxS/页面文件；禁用杀软/Defender；代关 VBS/核心隔离。
- 下载运行第三方优化器/exe。