# AGENTS.md — everything233

Agent 操作本仓库时遵守本文件。人读 README.md，机器读这里。

## 项目定位

voidtools Everything 的 Windows 替代。单文件静态 everything.exe，只搜文件名、不读内容。
仓库名 everything233 的 233 只是个人标识，产出 exe 必须叫 everything.exe，不要改成 everything233.exe。

## 硬性约束

- Windows-only；静态 CRT（-C target-feature=+crt-static），Windows 无 musl，crt-static 即等价物。
- 0 抽象：不引框架、不写 trait 层；能用 Windows API / 系统自带工具（curl、msiexec、taskkill）就不写依赖。
- 不读文件内容；白名单制（见下「扫描范围」），绝不整盘遍历；跳过清单：.git node_modules target Library Temp obj __pycache__ .gradle .nuget .cache，额外读 %APPDATA%\everything233\ignore.txt。
- 提权优先：NTFS 卷走 USN 日志直读（`FSCTL_ENUM_USN_DATA` 建索引 + 阻塞式 `FSCTL_READ_USN_JOURNAL` 增量），安装为 SYSTEM 计划任务（schtasks /ru SYSTEM /rl HIGHEST）。拿不到裸卷时回落目录遍历 + 30s 根签名轮询。
- ReFS/FAT 无法 MFT 枚举（`FSCTL_ENUM_USN_DATA` 返回 ERROR_INVALID_FUNCTION），只能一次性遍历，无增量；这类盘需 `everything serve --rebuild` 才看到新文件。这是文件系统限制，不是实现缺陷。

## 构建环境（每开新 shell 必设）

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
$env:LIB = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207\lib\x64'
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release
```

VS18 Community 缺 CRT 导入库，只借 BuildTools 的 libcmt/libucrt，link.exe 仍用 VS18。

## 代码结构

- src/main.rs — CLI 分发：serve / install / uninstall / update / query / probe / readtest；detach_self 后台分离；plog 同时写 stderr 与 `%ProgramData%\everything233\daemon.log`（SYSTEM 会话无控制台）。
- src/usn.rs — 裸卷句柄（FILE_FLAG_OVERLAPPED）+ `FSCTL_QUERY_USN_JOURNAL` / `FSCTL_ENUM_USN_DATA` / 异步 `FSCTL_READ_USN_JOURNAL`（事件等待+超时取消 → 空闲 0 CPU）。读盘必须固定 Min/MaxMajorVersion=2：V3 记录是 128 位 FileId，按 V2 解析会解析出 parent=0。增量续读必须用输出缓冲头 8 字节的 USN，不是最后一条记录的 Usn（StartUsn 含端点，用记录 Usn 会无限重读同一条）。
- src/index.rs — Rec 8B（parent u32: bit31 DIR / bit30 DEAD / 低30 父id；name_id u32）+ 名字链倒排（first/next，解锁只需扫唯一名表 ~65万 而非 400万 记录）+ frn（卷 MFT 号→rec，多卷用 base 偏移）。`Shared = Arc<RwLock<Index>>`：查询并发只读、写锁只在应用增量时短暂持有。build_usn 单遍枚举→邻接表→层序 BFS 套白名单前缀位掩码；build_walk 回退；spawn_usn 每卷一线程 LOWEST 优先级阻塞尾随；每 30s 落一次水位 sidecar。
- 单例：serve 开头先 `daemon::probe()` 探管道（跨会话，能识别 SYSTEM 会话里的实例），再 CreateMutexW(`everything233-single`) 兜底。
- src/daemon.rs — 命名管道 \\.\pipe\everything233，请求 query\nlimit\nflags，UTF-16 收发；SDDL `D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;WD)` 让会话 0 的 SYSTEM 服务能被普通用户查询；实例句柄复用（Connect/Disconnect 不重建），重建成环会 100% CPU。
- src/reg.rs — 提权走 schtasks（SYSTEM+HIGHEST，onstart），非提权回落 HKCU Run。`is_elevated()` 用 `\\.\PHYSICALDRIVE0` 探测：普通用户也能开 `\\.\C:` 只读做 FSCTL_QUERY_USN_JOURNAL，不能用它判管理员。
- src/ignore.rs — 内置跳过清单 + skip_name（USN 只有名字+父，只能按名字剪）+ ignore.txt。
- src/update.rs — 手动/自动更新 + 最快镜像路由。

## 扫描范围（改动前必读）

- 白名单制。`scan_roots()`：`%APPDATA%\everything233\include.txt` 逐行为扫描根（# 注释），支持盘符根（`D:\`）；缺省只扫 C: 用户办公目录（Desktop/Documents/Downloads/OneDrive/Favorites/Pictures/Videos/Music/source/scripts/DoubaoWork/WPS Cloud）。盘符根第一层由精确 TOP_SKIP 挡游戏库/缓存/临时/备份（勿用裸 game/games，会误伤 GameApp 等项目目录）。
- C: 系统树、其他盘、游戏库/缓存/临时/备份一律不进索引；数据盘不再整盘遍历。
- 忽略分两层：目录名 BUILTIN（ignore.rs）+ 完整路径 PREFIX（`\windows\` `\program files\` `\programdata\` `\appdata\` `\go\pkg\mod` `\.cargo\registry\` `\cargo\registry` `\node_modules`）。新增系统级缓存走 PREFIX，别往 BUILTIN 塞全局名。

## 内存布局与索引格式（改动前必读）

- Rec 8B：parent u32（bit31=DIR、bit30=DEAD 墓碑、低30=父 rec id，ROOT=0x3FFF_FFFF 为盘符锚点）、name_id u32。**不存 size/mtime**：命中时每条一次 `GetFileAttributesExW`（≤ limit 次调用），省 12B/条。
- 名字去重：build 期 seen HashMap 分配 name_id 后 clear+shrink；常驻只留 len(u16)-prefix 紧凑 names blob + name_off 偏移表。
- 名字链倒排：first（name_id→首条 rec+1）、next（rec→同名下一条）。重命名必须把记录重新挂到新名字链（`first[nid]` 头插），否则改名后搜不到；旧链残留用 `rec.name_id != nid` 守卫跳过。
- frn：卷 MFT 号→rec+1，多卷靠 VolMeta.base 偏移拼接；删除置 DEAD 并清 frn。
- 构建期不存完整路径；命中才沿 parent 链 resolve。
- 缓存格式 v5：magic `E233` + version=5 + names blob + name_off + recs(8B) + first + next + frn + vol 表（drive u8 / base u32 / journal_id u64 / next_usn i64）。load 校验 `recs.len()==next.len()`、`name_off.len()==first.len()`，不符返回 None 自动重建。
- 水位 sidecar `%LOCALAPPDATA%\everything233\usn.meta`（`E2UW` + drive/journal/next）：重启时用它补盘外变更，避免重写大缓存；journal 轮转/回绕（journal_id 变或 next_usn < first_usn）直接重新枚举，秒级，别拿旧索引凑合。
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