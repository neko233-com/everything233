# AGENTS.md — everything233

Agent 操作本仓库时遵守本文件。人读 README.md，机器读这里。

## 项目定位

voidtools Everything 的 Windows 替代。单文件静态 everything.exe，只搜文件名、不读内容。
仓库名 everything233 的 233 只是个人标识，产出 exe 必须叫 everything.exe，不要改成 everything233.exe。

## 文件系统支持红线（NTFS + ReFS 对等，违反即回滚）

**第一原则：索引能力按「卷」对等，不按「文件系统」打折。** 同一台机器上 NTFS 盘与 ReFS 盘，
用户看到的首建速度、增量延迟、空闲 CPU、常驻内存必须是同一档。允许内部两套机制，不允许外部两种体验。

| 维度 | NTFS | ReFS | 共同验收线 |
| --- | --- | --- | --- |
| 首建 | `FSCTL_ENUM_USN_DATA`（V2）单遍枚举 | 批量目录枚举 `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo=19)` | 冷启建库：NTFS ≤1s/百万条档；ReFS ≤30s/5.5M 条（本机 D: 实测 22~28s） |
| 增量 | `FSCTL_READ_USN_JOURNAL` 阻塞读（0 CPU / 亚秒唤醒） | `FSCTL_READ_USN_JOURNAL` 非阻塞轮询 1s（`BytesToWaitFor=0`） | 变更可见 ≤2s（NTFS <0.2s，ReFS ≤1.2s） |
| 空闲开销 | 线程阻塞在事件上 | 1s 一次 syscall | 空闲 CPU = 0（10s 采样 0.0s） |
| 内存 | 名字链倒排 + frn 稠密表 | 同结构 + `dirs`(u128→rec) | RSS ≤150MB / 5.5M 条（本机 140MB） |

- **NTFS 路径**：`ENUM_USN_DATA` 建索引 → `READ_USN_JOURNAL` 阻塞尾随。键 = MFT 号，
  `usn::mft(frn)` = 低 32 位（V2 的 64 位 frn 高 16 位是序列号，**禁止**把 raw u64 直接当数组下标，会索引越界并静默丢弃全部增量）。
- **ReFS 路径**：ReFS 的 `ENUM_USN_DATA` 返回 `ERROR_INVALID_FUNCTION(1)`（MFT 枚举是 NTFS-only），
  但 `QUERY_USN_JOURNAL` / `READ_USN_JOURNAL` 可用。所以首建走「批量目录枚举」，
  一次 syscall 拿整目录的 name + attrs + 128 位 FILE_ID；**子目录 FileId 直接继承父目录列表结果，不要逐目录 `FileIdInfo`**
  （600k+ 次 syscall 会把建库从 22s 拖到 152s）。增量请求 **V3** 记录，键 = 原始 16 字节 FILE_ID 原样使用（`u128` LE）——
  目录枚举与 journal 两侧字节天然一致，不要试图解释那个怪异的 lo/hi 分布。
- **两条死路，写进红线**：
  1. ReFS 上阻塞式读 journal（`BytesToWaitFor>0`）：`Timeout` 被忽略，阻塞 30~101s 后返回 `ERROR_INVALID_USER_BUFFER(1784)`。ReFS 只能用 `BytesToWaitFor=0` 轮询。
  2. ReFS 请求 V2 记录与 64 位打包 id 做 join：ReFS id 是 128 位，V2 打包值与目录枚举值对不上。必须 V3。
- **记录解析必须按 `MajorVersion` 分派**（V2 / V3 共用一套字段偏移表）：
  V2 `FileNameLength@56`、V3 `FileNameLength@72`，**两者都是 WORD**；当 DWORD 读会把长度读成 4 字节垃圾 → 解析出 0 条（历史上踩过两次）。
- **架构最优解原则**：双文件系统分派只允许「一个 `ver`/`poll`/`walk` 标志 + 分支」，共享 Change/Rec/名字链/缓存格式。
  **不许**为"支持多种文件系统"引入 trait / 抽象层 / 多后端注册表（违反 0 抽象）。想合并两条路径，先拿出实测数据证明任一盘不退化。
- FAT/exFAT 等无 journal 的卷：批量目录枚举首建 + stat 校验存活（删除靠查询期 `stat_path` 自然消失），允许无增量，但必须在日志里显式标注为 walk 卷。

## 硬性约束

- Windows-only；静态 CRT（-C target-feature=+crt-static），Windows 无 musl，crt-static 即等价物。
- 0 抽象：不引框架、不写 trait 层；能用 Windows API / 系统自带工具（curl、msiexec、taskkill）就不写依赖。
- 不读文件内容；白名单制（见下「扫描范围」），绝不整盘遍历；跳过清单：.git node_modules target Library Temp obj __pycache__ .gradle .nuget .cache，额外读 `%APPDATA%\everything233\ignore.txt`。
- 提权优先：安装为 SYSTEM 计划任务（schtasks /ru SYSTEM /rl HIGHEST）。拿不到裸卷时回落目录遍历 + 30s 根签名轮询。
- 增量失败不允许降级成"每 N 分钟全盘重扫"：journal 不可用才重建，重建必须 ≤30s（ReFS 验收线）。

## 构建环境（每开新 shell 必设）

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;C:\Program Files\Git\cmd;$env:SystemRoot\System32;$env:PATH"
$env:LIB = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207\lib\x64;C:\Program Files (x86)\Windows Kits\10\lib\10.0.22621.0\um\x64;C:\Program Files (x86)\Windows Kits\10\lib\10.0.22621.0\ucrt\x64'
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --offline
```

VS18 Community 缺 CRT 导入库，只借 BuildTools 的 libcmt/libucrt，link.exe 仍用 VS18。

## 代码结构

- src/main.rs — CLI 分发：serve / install / uninstall / update / query / probe / readtest / build / readprobe / jprobe / walkprobe / idprobe；detach_self 后台分离；plog 同时写 stderr 与 `%ProgramData%\everything233\daemon.log`（SYSTEM 会话无控制台）。
- src/usn.rs — 裸卷句柄（`FILE_FLAG_OVERLAPPED`）+ `QUERY_USN_JOURNAL` / `ENUM_USN_DATA` / `READ_USN_JOURNAL`。
  `Volume{ver, poll}`：NTFS→V2+阻塞；其它→V3+`read_poll`（1s 轮询，一次 syscall，空结果才 sleep）。
  `parse()` 版本无关；`decode()` 统一推进 `next_usn`；`mft()` 取 MFT 号；`read_probe`/`dump_from` 是布局诊断入口。
  增量续读必须用输出缓冲头 8 字节的 USN，不是最后一条记录的 Usn（StartUsn 含端点，用记录 Usn 会无限重读同一条）。
  `ERROR_HANDLE_EOF(38)` 是正常收尾，不要当错误打日志。
- src/index.rs — Rec 8B（parent u32: bit31 DIR / bit30 DEAD / 低30 父id；name_id u32）+ 名字链倒排（first/next，解锁只需扫唯一名表 ~65万 而非 400万 记录）。
  `frn`：卷 MFT 号→rec，多卷靠 `VolMeta.base` 偏移拼接（NTFS 专用）；`dirs`：128 位 FileId→rec（walk 卷专用）；`walk`：走查型盘符集合，`apply` 按它分派。
  `Shared = Arc<RwLock<Index>>`：查询并发只读、写锁只在应用增量时短暂持有。
  build_usn 单遍枚举→邻接表→层序 BFS 套白名单前缀位掩码；`walk_bulk` 批量目录枚举（`walk_plain` 为 FindFirstFile 兜底，失败计数并打印）；`spawn_usn` 每卷一线程 LOWEST 优先级（NTFS 阻塞 / ReFS poll）；每 30s 落一次水位 sidecar。
  改名配对：同批 `RENAME_OLD_NAME` 记 `(key,rec)`，随后的 `RENAME_NEW_NAME` 原地改名并重挂名字链（walk 卷文件没有 id 表，靠这条配对才能改名）。
  walk 卷的重复 CREATE（create/data-extend/close 会重放同名）必须先扫同名链去重，否则一条文件变三条。
- 单例：serve 开头先 `daemon::probe()` 探管道（跨会话，能识别 SYSTEM 会话里的实例），再 CreateMutexW(`everything233-single`) 兜底。
- src/daemon.rs — 命名管道 \\.\pipe\everything233，请求 query\nlimit\nflags，UTF-16 收发；SDDL `D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;WD)` 让会话 0 的 SYSTEM 服务能被普通用户查询；实例句柄复用（Connect/Disconnect 不重建），重建成环会 100% CPU。
- src/reg.rs — 提权走 schtasks（SYSTEM+HIGHEST，onstart），非提权回落 HKCU Run。`is_elevated()` 用 `\\.\PHYSICALDRIVE0` 探测：普通用户也能开 `\\.\C:` 只读做 FSCTL_QUERY_USN_JOURNAL，不能用它判管理员。
- src/ignore.rs — 内置跳过清单 + skip_name（USN 只有名字+父，只能按名字剪）+ ignore.txt。
- src/update.rs — 手动/自动更新 + 最快镜像路由。

## 扫描范围（改动前必读）

- 白名单制。`scan_roots()`：`%APPDATA%\everything233\include.txt` 逐行为扫描根（# 注释），支持盘符根（`D:\`）；缺省只扫 C: 用户办公目录（Desktop/Documents/Downloads/OneDrive/Favorites/Pictures/Videos/Music/source/scripts/DoubaoWork/WPS Cloud）。盘符根第一层由精确 TOP_SKIP 挡游戏库/缓存/临时/备份（勿用裸 game/games，会误伤 GameApp 等项目目录）。
- **SYSTEM 守护读的是机器级 `C:\ProgramData\everything233\include.txt`，不是用户的 `%APPDATA%`**：改白名单要两处都确认，否则"改了没效果"。
- C: 系统树、其他盘、游戏库/缓存/临时/备份一律不进索引；数据盘不再整盘遍历。
- 忽略分两层：目录名 BUILTIN（ignore.rs）+ 完整路径 PREFIX（`\windows\` `\program files\` `\programdata\` `\appdata\` `\go\pkg\mod` `\.cargo\registry\` `\cargo\registry` `\node_modules`）。新增系统级缓存走 PREFIX，别往 BUILTIN 塞全局名。

## 内存布局与索引格式（改动前必读）

- Rec 8B：parent u32（bit31=DIR、bit30=DEAD 墓碑、低30=父 rec id，ROOT=0x3FFF_FFFF 为盘符锚点）、name_id u32。**不存 size/mtime**：命中时每条一次 `GetFileAttributesExW`（≤ limit 次调用），省 12B/条；同时充当 ReFS 存活校验（stat 失败即丢该命中）。
- 名字去重：build 期 seen HashMap 分配 name_id 后 clear+shrink；常驻只留 len(u16)-prefix 紧凑 names blob + name_off 偏移表。
- 名字链倒排：first（name_id→首条 rec+1）、next（rec→同名下一条）。重命名必须把记录重新挂到新名字链（`first[nid]` 头插），否则改名后搜不到；旧链残留用 `rec.name_id != nid` 守卫跳过。
- frn：NTFS 卷 MFT 号→rec+1，多卷靠 VolMeta.base 偏移拼接；删除置 DEAD 并清 frn。
- dirs：walk 卷的 128 位目录 FileId→rec+1；journal 新建的目录 id 由 create 事件补进来（两层新目录下的新文件因此能解析到完整路径）。
- 构建期不存完整路径；命中才沿 parent 链 resolve。
- 缓存格式 v6：magic `E233` + version=6 + names blob + name_off + recs(8B) + first + next + frn + vol 表（drive u8 / base u32 / journal_id u64 / next_usn i64）+ walk 列表 + dirs(20B/条: u128 + u32)。load 校验 `recs.len()==next.len()`、`name_off.len()==first.len()`、`dirs.len()==dirs 元数据`，不符返回 None 自动重建。
- 水位 sidecar `%LOCALAPPDATA%\everything233\usn.meta`（`E2UW` + drive/journal/next）：重启时用它补盘外变更，避免重写大缓存；journal 轮转/回绕（journal_id 变或 next_usn < first_usn）直接重新枚举，秒级（ReFS ≤30s），别拿旧索引凑合。
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

## 验证与验收（改索引/增量必跑）

- `everything build`（提权）：打印每卷记录数、耗时、journal/walk 标记。NTFS 必须 <1s/百万条，ReFS <30s/5.5M。
- `everything readtest C|F|D`：打印版本、next_usn、每批变更的 frn/parent/attrs/reason/name。NTFS 应表现为"阻塞很久→一批"，ReFS 应表现为"1s 一批"。
- `everything readprobe D` / `jprobe D 3` / `walkprobe 'D:\Code'` / `idprobe <path...>`：journal 布局与 FileId 半部诊断。
- 实时验收（本机脚本 C:\Windows\Temp\e233_refs_test.ps1 / e233_ntfs2.ps1）：建目录、建文件、改名（旧名必须消失）、删除、两层新目录下建文件，全部 ≤2s 可见；随后 8~10s 空闲 CPU=0。
- 提权测试必须经 `Start-Process -Verb RunAs`（本机 PowerShell 上下文默认非提权，`usn::open` 返回 err 5）；`-Wait` 直接调 powershell 会以 STATUS_CONTROL_C_EXIT 静默死，必须 `cmd /c powershell -NoProfile -ExecutionPolicy Bypass -File <script> > D:\x.log 2>&1` 包一层，日志写用户可读盘符（`C:\Windows\Temp\*.log` 非提权读不了）。

## 打包与发版

- MSI：WiX v7，wix build packaging\Product.wxs -o dist\everything233.msi -arch x64（cwd=仓库根）。
- 资产：everything233.msi + everything233-windows-x64.zip（内含 everything.exe）。
- 版本号改 Cargo.toml，同步 packaging\Product.wxs 的 Version（两处不同步 = 发版事故）。
- 用户说「发布」= 自动 cargo build → 打 MSI/zip → gh release create vX.Y.Z 传资产，无需再确认。
- install.ps1 从 latest release 拉 MSI，UAC 装，写用户 PATH，装完自注册并起 daemon。

## 禁止

- 删个人目录文件；碰回收站/Windows.old/还原点需用户单项确认。
- 改 System32/WinSxS/页面文件；禁用杀软/Defender；代关 VBS/核心隔离。
- 下载运行第三方优化器/exe。
- 正式构建的热路径留临时调试日志（apply/tail 里 `dbg ...` 必须删干净；诊断走 CLI 子命令）。
- ReFS 上阻塞读 journal、逐目录 `FileIdInfo`、用 V2 记录做 128 位 join —— 三条均已实测失败，不要"再试一次"。
- 把 raw 64 位 frn 直接当 `frn` 数组下标（必须先 `usn::mft()`）。
