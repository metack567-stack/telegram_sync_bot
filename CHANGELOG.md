# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Semantic Versioning](https://semver.org).

<!--
Note: In this file, do not use the hard wrap in the middle of a sentence for compatibility with GitHub comment style markdown rendering.
-->

## [Unreleased]

### Added

- 下载前检查磁盘剩余空间，空间不足快速失败，避免写满磁盘
- 同名文件自动追加 `_1`/`_2` 递增后缀，互不覆盖（便于刮削程序识别）
- 并发下载限制（`DOWNLOAD_CONCURRENCY`，默认 3）
- bypass key 持久化到 `bypass.key`，重启后密钥不变；`/bypasskey` 命令需通过认证
- 启动时向 Telegram 注册命令菜单（`/start` `/help` `/state` `/toggle` `/bypasskey`）
- 重启后自动恢复中断的下载任务，下载完成自动分类到对应目录
- trash 目录自动清理（`TRASH_RETENTION_DAYS`，默认 7 天，每小时检查）
- photo 使用 caption 作为文件名（清洗非法字符后加 `.jpg`，空 caption 回退 file_id）
- 数据库与下载缓存目录分离（`DB_DIR` / `SERVER_CACHE_DIR`）
- 新增 `/clear` 命令：一键清空 normal 目录（含确认步骤，不影响 fav/trash 与 TG 消息）
- 新增 `DELETE_UNKNOWN_MESSAGES` 环境变量：对未跟踪消息点表情时删除该消息，默认关闭（保护共享频道中机器人未管理的消息）
- 新增 `RESET_DB` 环境变量：调试用，设置后启动时重建数据库表（默认只迁移，不再清库）
- photo caption 文件名截断到 80 字符，避免超长文件名
- 新增 sqmusic 音乐联动 `/music <歌名>` 命令：搜索（默认酷我源）→ 回复数字选歌 → sqmusic 后台下载到音乐库（Emby 兼容目录）→ 音频文件回传 Telegram（`SQMUSIC_URL` / `SQMUSIC_USER` / `SQMUSIC_PASS` / `MUSIC_DIR` 环境变量控制）
- 新增 Emby 音乐库下载前预查：`/music` 下载前先查 Emby（`EMBY_URL` / `EMBY_API_KEY`），音乐库已有该歌则直接回传现有文件、不重复下载；Emby 未配置或查询失败时回退到本地目录预查
- 新增 `/emby <歌名>` 命令：搜索 Emby 音乐库、序号按钮点播，把库里的音频文件直接发回 Telegram（60 秒内有效）
- `/music` 下载完成新歌后自动触发 Emby 音乐库扫描（`Library/Refresh`），无需手动刷新
- 新增 Emby 歌单：`/playlist <歌单名>` 创建/打开 Emby 播放列表；`/emby` 搜索结果点 ➕ 一键把歌加入歌单（Emby 音乐库直接可播）
- `/music` 搜索结果同样支持 ➕ 一键加入当前歌单：歌曲需已在 Emby 音乐库（未下载/未扫描时提示先下载）
- **试听先行**：新增 `MUSIC_TMP_DIR` 临时试听区（如 `/music-tmp`）。`/music` 下载先落临时区不入库，音频回传 + 操作面板 `[📥 入库] [❤️ 收藏] [🗑 删除]`；入库=移入音乐库并自动刷新 Emby，收藏=写 SQLite（文件保留），删除=删临时文件；超时未操作由后台任务定时清理（`MUSIC_TMP_RETENTION_SECS` 默认 86400s、`MUSIC_TMP_KEEP` 默认 50）
- 新增 `/favs` 音乐收藏命令：收藏列表（含专辑/收藏时间），点序号播放、随时取消收藏（favorites 表持久化）
- `/playlist` 支持查看歌单内歌曲（`playlist_items`），点序号直接在 Telegram 播放；入库后的歌单里可继续 ➕ 加歌
- `/playlist <歌单名>` 就绪后直接列出歌单内已有歌曲（点序号播放），空歌单给出加歌指引，不再只显示"已就绪"提示
- `/playlist` 无参数时直接列出 Emby 全部歌单（按钮点选打开，当前歌单标记 ✅），不用再输入歌单名
- 歌单列表按钮改为纯序号（横向一排，每行最多 8 个），点序号打开歌单；歌单歌曲列表底部新增 `🔙 返回歌单列表`
- `➕ 加入歌单` 改为弹出歌单列表当场选择（点序号加入对应歌单），不再固定加入"当前歌单"；选择后记住为当前歌单
- 新增歌单维护：`/playlist` 列表底部 `🗑 删除歌单` → 点序号 → `✅ 确认删除 / ❌ 取消` 两步确认（`DELETE /Items/{id}`），删除的是当前歌单时自动清除选中
- 新增删除已入库歌曲：`/emby` 搜索结果面板底部 `🗑 删除歌曲` → 点序号 → `✅ 确认删除 / ❌ 取消` 两步确认（`DELETE /Items/{id}` + 自动刷新音乐库）；**会连同音乐文件一起删除、不可恢复**；删除后候选列表自动移除该歌，`🔙 返回` 可回到搜索结果面板

### Changed

- 大文件下载不再设置时间上限：本地 server 缓存持续写入则无限等待，连续约 1 分钟无活动才判定失败
- 下载失败自动重试（最多 3 次），失败/取消时清理下载目录中的半成品文件
- 日志带本地时区时间戳（TZ=Asia/Shanghai）
- bypass key 恒为 16 位（不再因构建类型而缩短）
- 回收站清理按文件进入回收站的时间（ctime）计算保留期，而非下载时间（mtime）
- bot 构建镜像升级至 Rust 1.88（clippy 告警全部清理）

### Fixed

- 媒体组原消息删除失败不再中断整个下载流程（记录 warn 后继续）
- 数据库中出现未知状态值不再 panic，回退到默认状态
- 本地 server 缓存路径解析、`cp` 在 docker / podman 下的兼容
- debug（非 release）构建启动不再清空数据库（原实现 debug 下会重建表导致数据丢失）
- `try_multiple_times(0)` 不再下溢 / 死循环
- 用户取消的下载不再被误标为失败
- 后台任务（下载状态同步、文件分类等待、消息处理）出错或 panic 现在会记录日志
- `summarize` / `clear` 统计按 inode 去重，硬链接文件不再重复计算字节

## [0.5.3] - 2026-01-30


- fix typo

## [0.5.2] - 2025-03-14

- fix bug: duplicate extension in file_name

## [0.5.1] - 2025-03-14

- support audio
- better file_name extraction

## [0.5.0] - 2025-03-14

- support kubernetes
- the unique volume of host to container in server and bot
- try hard-linking when move file from local server to bot-output in local-server mode
- use `data` instead of `output` as the argument name
- `-f` for favorite, `-F` for dislike

## [0.4.0] - 2025-03-12

- move from `sqlx` to `sea-orm`
- improve with a download manager
- play with `CancellationToken`, better code structure
- better file-state and transport-state management
- improve with foreign key
- sub command to delete file/msg from fs, db and telegram
- split group msgs
- TryMultipleTimers trait to lift success possibility

## [0.3.2] - 2025-03-10

- command `/togglesync` to stop saving new files and only works as a reaction handler
- sql improvements
- fix bug: do not need unpin deleted message

## [0.3.1] - 2025-03-10

- fix bug: the bot will check the path exists before operate
- fix bug: delete the message out of control
- log more detailed Context
- pin while fav, unpin while unfav or delete
- improve direct to bot msg experience

## [0.3.0] - 2025-03-10

- support channel management: the bot will generate a dynamic bypass password, use `/unpause <password>` to unpause the bot
- rename env var `OWNER_ID` to `BYPASS_USERS`
- do not remove the file, move to trash instead
- move from `sled` to `sqlite`

## [0.2.5] - 2025-03-09

- speed up build with `ninja`
- the bot image will stop with SIGINT
- the server image will wait 5 seconds before exit

## [0.2.4] - 2025-03-07

- kube play support
- cautions: user updated to this version should reload or rebuild the images

## [0.2.3] - 2025-03-07

- release images
- improve document
- improve Containerfile

## [0.2.2] - 2025-03-07

- fix bug in replying

## [0.2.1] - 2025-03-07

- log when download start
- fix bug: now loop GetFile

## [0.2.0] - 2025-03-07

- local server support

## [0.1.5] - 2025-03-06

- document download support

## [0.1.4] - 2025-03-06

- async download

## [0.1.3] - 2025-03-06

- use emoji to manage files

## [0.1.2] - 2025-03-05

- reply after downloading

## [0.1.1] - 2025-03-05

- sha256 file name
- systemd service example

## [0.1.0] - 2025-03-05

- MVP
