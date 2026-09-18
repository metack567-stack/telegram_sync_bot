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

### Changed

- 大文件下载不再设置时间上限：本地 server 缓存持续写入则无限等待，连续约 1 分钟无活动才判定失败
- 下载失败自动重试（最多 3 次），失败/取消时清理下载目录中的半成品文件
- 日志带本地时区时间戳（TZ=Asia/Shanghai）

### Fixed

- 媒体组原消息删除失败不再中断整个下载流程（记录 warn 后继续）
- 数据库中出现未知状态值不再 panic，回退到默认状态
- 本地 server 缓存路径解析、`cp` 在 docker / podman 下的兼容

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
