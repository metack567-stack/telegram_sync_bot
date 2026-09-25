# telegram_sync_bot

一个将 Telegram 文件自动同步到本地服务器（NAS）的机器人。把文件转发给机器人即可自动下载保存；配合托管频道，还能用表情投票实现多人协作文档归档。基于 **Rust + Teloxide** 构建，镜像仅约 **39MB**，轻量高效。

## 功能特性

- **一键下载**：把文件转发给机器人，自动下载并保存到指定目录
- **突破 20MB 限制**：内置本地 Telegram API Server（telegram-bot-api），支持超大文件
- **表情快速管理**（个人使用）：对消息点 👍/❤ 收藏到 `fav/`，点 👎 移入回收站
- **频道表情投票**（团队使用）：多人用表情打分，机器人按得分自动归档、置顶或删除
- **下载文件自动分类**：按类型归档到 `images / videos / audio / documents / other` 子目录
- **回收站自动清理**：过期文件（默认 7 天）每小时自动清除
- **断点恢复**：重启后自动恢复未完成的下载任务
- **硬链接归档**：文件以硬链接方式组织，不重复占用磁盘空间
- **sqmusic 音乐联动**：`/music <歌名>` 搜索并下载歌曲（对接本机 sqmusic「简单音乐」）。**试听先行**：下载落到临时区，先回传音频试听，满意后一键 **📥入库**（移入音乐库 + 自动刷新 Emby）/ **❤️收藏** / **🗑删除**，不满意删临时文件即可，不污染音乐库
- **Emby 音乐库联动**：下载前预查音乐库避免重复下载；`/emby <歌名>` 直接查库点播（把库里的歌发回 Telegram）；下载完成自动触发 Emby 库刷新；`/emby` 搜索结果面板底部 `🗑 删除歌曲` 两步确认即可删除已入库歌曲（**连同音乐文件一起删除，不可恢复**）
- **Emby 歌单**：`/playlist <歌单名>` 创建/打开 Emby 播放列表；`/emby` 搜索结果点 ➕ 一键把歌加入歌单；`/playlist` 列出全部歌单（序号按钮点选）、可查看歌单内歌曲并点序号直接播放，列表底部 `🗑 删除歌单` 两步确认即可删除；入库后点 `➕ 加入歌单` 会弹出歌单列表当场选择
- **音乐收藏**：`/favs` 查看收藏列表，点序号播放、随时取消收藏（SQLite 持久化，跨重启保留）
- **多态部署**：支持 Docker Compose / Docker / Podman / Kubernetes / 原生 Systemd

## 工作原理

### 1. 直接发送给机器人

文件发送给机器人后自动下载保存。所有者可用表情管理文件：

| 操作 | 表情 | 效果 |
| --- | --- | --- |
| 收藏 | 👍 / ❤ | 移动到 `fav/`（收藏目录） |
| 删除 | 👎 | 移动到 `trash/`（回收站） |

> 对机器人未跟踪的消息（例如共享频道里别人发的消息）点表情时，**默认不会删除**该消息；如需此行为，设置 `DELETE_UNKNOWN_MESSAGES=on`。

### 2. 发送到机器人托管频道

先发送 `/toggle <bypasskey>` 切换机器人的工作状态（`<bypasskey>` 在日志中可见，`/bypasskey` 可重新打印）：

| 状态 | 行为 |
| --- | --- |
| `paused` | 暂停，不响应任何文件 |
| `active` | 同步文件 + 响应表情回应 |
| `partially active` | 仅响应表情回应，不同步文件 |

下载过程中机器人会给消息设置 🫡，完成后改为 👌（失败 😭 / 取消 😨 / 内部错误 👾）。

频道成员用表情给文件打分，机器人统计得分后自动处理：

| 表情 | 得分 |
| --- | --- |
| 👍😁🙏😇🤗 | +1 |
| ❤🔥🥰🎉🍌💋💘😘 | +2 |
| 👎🤯😱😢🥴🌚😐🖕😨 | -1 |
| 🤬🤮💩🤡💔😡 | -2 |

- 得分 `>= fav_score_limit`（默认 10）→ 归档到 `fav/` 并置顶
- 得分 `< dislike_score_limit`（默认 -10）→ 归档到 `trash/` 并从频道删除
- 其余 → 归档到 `normal/` 并取消置顶

> 注意：Telegram 的表情计数更新有几秒到几分钟延迟，频道场景下的归档会稍晚生效。

## 文件组织结构

```
data/                          # 数据目录（-d 指定）
├── 663919342/                 # 按聊天 ID 分目录
│   ├── normal/                # 普通文件（自动分类）
│   │   ├── images/            #   图片 jpg/png/gif/webp…
│   │   ├── videos/            #   视频 mp4/mkv/webm…
│   │   ├── audio/             #   音频 mp3/flac/m4a…
│   │   ├── documents/         #   文档 pdf/zip/docx…
│   │   └── other/             #   其他
│   ├── fav/                   # 收藏
│   └── trash/                 # 回收站（超期自动清理）
└── *.part / 临时文件           # 下载中（完成后移入分类目录）
```

## 环境变量（.env）

| 变量 | 必填 | 说明 | 默认值 |
| --- | --- | --- | --- |
| `TELOXIDE_TOKEN` | ✅ | 从 [BotFather](https://t.me/BotFather) 获取的机器人 Token | - |
| `BYPASS_USERS` | - | 可管理的 Telegram 用户 ID，多个用逗号分隔 | 不限制 |
| `TELEGRAM_API_ID` | * | 本地服务器模式：Telegram API ID | - |
| `TELEGRAM_API_HASH` | * | 本地服务器模式：Telegram API Hash | - |
| `TELEGRAM_DATA_DIR` | * | 容器部署：数据目录挂载路径 | - |
| `APP_DATA_DIR` | * | 容器部署：应用数据（数据库）挂载路径 | - |
| `DB_DIR` | - | 数据库（data.db）与 bypass.key 存放目录 | 同数据目录 |
| `SERVER_CACHE_DIR` | - | telegram-bot-api 本地缓存根目录 | - |
| `TRASH_RETENTION_DAYS` | - | 回收站文件保留天数（每小时检查清理） | 7 |
| `DOWNLOAD_CONCURRENCY` | - | 同时下载的任务数 | 3 |
| `DELETE_UNKNOWN_MESSAGES` | - | 对未跟踪消息点表情时删除该消息（`on`/`1`/`true`/`yes` 开启） | 关闭（不删除） |
| `RESET_DB` | - | 调试用：设置任意值后启动时重建数据库表（**会清空全部数据**） | 不设置（只迁移，不清库） |
| `SQMUSIC_URL` | * | sqmusic 后端地址（启用 `/music` 联动），如 `http://sqmusic_main:8099` | 不设置（功能关闭） |
| `SQMUSIC_USER` | * | sqmusic 登录用户名 | `admin` |
| `SQMUSIC_PASS` | * | sqmusic 登录密码 | `admin` |
| `MUSIC_DIR` | * | sqmusic 音乐库目录在 bot 容器内的挂载路径（如 `/music`），用于入库与回传文件 | 不设置（功能关闭） |
| `MUSIC_TMP_DIR` | - | 临时试听区目录（如 `/music-tmp`）。设置后 `/music` 下载先落临时区试听（不入库），点 📥 入库才移入音乐库；后台定时清理过期文件 | 不设置（下载直接进音乐库，旧行为） |
| `MUSIC_TMP_RETENTION_SECS` | - | 临时试听区文件保留秒数（超时自动清理，每小时检查） | 86400（1 天） |
| `MUSIC_TMP_KEEP` | - | 临时试听区最少保留的文件数（超过则删最旧） | 50 |
| `EMBY_URL` | * | Emby 服务地址（启用 `/music` 下载前音乐库预查），如 `http://192.168.8.219:9096/emby` | 不设置（功能关闭） |
| `EMBY_API_KEY` | * | Emby API 密钥（查询与删除用，需有 DELETE 权限的后台 API Key；浏览器登录 token 删除会 403） | 不设置（功能关闭） |

`*` 使用本地服务器模式（无 20MB 限制）时需要。API ID / Hash 在 [Telegram 官网](https://core.telegram.org/obtaining_api_id) 申请（申请报错时可尝试 `cloudflare warp` 代理）。

> sqmusic 联动（`/music`）：需同时设置 `SQMUSIC_URL` 与 `MUSIC_DIR`。用法：向机器人发送 `/music 晴天 周杰伦`，点下方序号选歌（横向一排），再点音质按钮（可点 ⚙️ 自动），机器人调 sqmusic 搜索下载。**设置 `MUSIC_TMP_DIR` 后走"试听先行"流程**：下载落临时区不入库，音频回传试听 + 操作面板 `[📥 入库] [❤️ 收藏] [🗑 删除]`——入库 = 文件移入音乐库并自动触发 Emby 刷新；收藏 = 写入 SQLite（试听文件保留）；删除 = 删临时文件；超时未操作由后台任务定时清理（`MUSIC_TMP_RETENTION_SECS`/`MUSIC_TMP_KEEP`）。下载前会先查 Emby 音乐库（配置 `EMBY_URL`/`EMBY_API_KEY` 时）与本地音乐目录：已有该歌则直接回传现有文件，不重复下载。另有 `/emby <歌名>` 命令：直接搜索 Emby 音乐库点播（不经过下载），结果面板底部 `🗑 删除歌曲` 可两步确认删除已入库歌曲（**会连同音乐文件一起删除、不可恢复**）；`/playlist <歌单名>` 创建/打开 Emby 歌单，`/playlist`（无参数）列出全部歌单点序号打开、底部可删除歌单，`/emby` 搜索结果点 ➕ 加入歌单；入库后 `➕ 加入歌单` 会弹出歌单列表当场选择；`/favs` 查看收藏列表，点序号播放、可取消收藏。

## 部署

### 方式一：Docker Compose（推荐）

`docker-compose.yml`：

```yaml
services:
  server:
    image: ghcr.io/metack567-stack/telegram_sync_bot/server:latest
    container_name: tgsync-server
    restart: unless-stopped
    env_file: .env
    ports:
      - "8081:8081"
    volumes:
      - ${TELEGRAM_DATA_DIR}:/app/data

  bot:
    image: ghcr.io/metack567-stack/telegram_sync_bot/bot:latest
    container_name: tgsync-bot
    restart: unless-stopped
    depends_on:
      - server
    env_file: .env
    environment:
      - TZ=Asia/Shanghai
      - SERVER_CACHE_DIR=/app/data/server-cache
      - DB_DIR=/app/db
    volumes:
      - ${TELEGRAM_DATA_DIR}:/app/data
      - ${APP_DATA_DIR}/db:/app/db
    networks:
      - default
      - sqmusic        # 仅启用 /music 联动时需要（外部网络，指向 sqmusic 的 compose 网络）
    stop_signal: SIGINT
    command: run -d /app/data -l http://server:8081

networks:
  sqmusic:
    external: true
    name: sqmusic_sq-app-network
```

启用 `/music` 联动时，把 sqmusic 的音乐库挂载给 bot 容器（只读即可），并在 `.env` 配置 sqmusic 连接信息：

`.env` 额外配置：

```
TELEGRAM_DATA_DIR=/path/to/data
APP_DATA_DIR=/path/to/app-data

# 启用 /music 联动（可选）
SQMUSIC_URL=http://sqmusic_main:8099
SQMUSIC_USER=admin
SQMUSIC_PASS=admin
MUSIC_DIR=/music
# 临时试听区（可选，推荐）：下载先落临时区，满意后点 📥 入库
MUSIC_TMP_DIR=/music-tmp
# 启用下载前 Emby 音乐库预查（可选）
EMBY_URL=http://192.168.8.219:9096/emby
EMBY_API_KEY=your_emby_api_key
```

同时给 bot 服务追加音乐库挂载：`- /vol1/1000/音频/音乐:/music:rw`、`- /vol1/1000/Docker/telegram-sync-bot/music-tmp:/music-tmp`（路径按你的实际目录调整；sqmusic 的下载目录需同样指向 `/music-tmp`）。

启动 / 更新：

```sh
docker compose up -d                                  # 启动
docker compose pull && docker compose up -d           # 更新到最新镜像
```

> 镜像托管在 GitHub Container Registry：`ghcr.io/metack567-stack/telegram_sync_bot/{bot,server}`；也可本地构建，见下文。

### 方式二：Docker run

```sh
# 构建镜像（或直接拉取 GHCR 上的 bot:dev / server:dev）
docker build -f bot/Containerfile --target bot -t bot:0.X.0 bot
docker build -f server/Containerfile --target server -t server:latest server

# 创建网络并启动本地服务器
docker network create tgsync
docker run --name server --network tgsync -itd --env-file .env -p 8081:8081 \
    -v /path/to/data:/app/data server

# 启动机器人
docker run --name bot --network tgsync -itd --env-file .env --stop-signal SIGINT \
    -v /path/to/data:/app/data -v /path/to/db:/app/db \
    bot:0.X.0 run -d /app/data -l http://server:8081
```

### 方式三：Podman

**镜像获取**（二选一）：

```sh
# 1) 从 Release 页面下载 server.tar.gz / bot.tar.gz 加载
podman load -i server.tar.gz
podman load -i bot.tar.gz

# 2) 或本地构建（bot 需 Rust 工具链；server 构建耗时较长）
podman build --target bot -t bot:$(cargo pkgid -p telegram_sync_bot | sed -n "s/.*@//p") --network host bot
podman build --target server -t server --network host server
```

**a) 常规：服务器容器 + 机器人原生**

```sh
podman run --name server -itd --env-file .env -p 8081:8081 server
telegram_sync_bot run -d /path/to/output -l http://127.0.0.1:8081 -c podman -i server
```

**b) Pod 方式**

```sh
podman pod create sync_bot
podman run --pod sync_bot --name server -itd --env-file .env \
    -v /path/to/data:/app/data server
podman run --pod sync_bot --name bot -itd --env-file .env --stop-signal SIGINT \
    -v /path/to/data:/app/data \
    bot:0.X.0 run -d /app/data -l http://server:8081
```

**c) Podman kube play**

修改 `sync-bot.yaml` 适配你的环境：

```sh
podman kube play sync-bot.yaml
```

**d) Kubernetes**

修改 `.env` 和 `k8s/pv.yaml` 等文件：

```sh
sudo ctr -n=k8s.io images import /tmp/server.tar.gz
sudo ctr -n=k8s.io images import /tmp/bot.tar.gz
kubectl apply -k .
```

### 方式四：原生运行（无需容器）

**20MB 限制模式**（直接使用 Telegram 官方 API）：

```sh
telegram_sync_bot run -d /path/to/data
```

**无限制模式**（本地 server 原生运行，省略 `-c`/`-i` 参数即可）：

```sh
telegram_sync_bot run -d /path/to/output -l http://127.0.0.1:8081
```

### Systemd 托管

```ini
# /etc/systemd/system/sync-bot.service
[Unit]
Description=Telegram file sync bot
After=network-online.target

[Service]
Type=simple
User=<...>
WorkingDirectory=</path/to/output>
ExecStart=/usr/local/bin/telegram_sync_bot run
Restart=on-failure
Environment="TELOXIDE_TOKEN=<...>"
Environment="BYPASS_USERS=<...>"

[Install]
WantedBy=multi-user.target
```

```sh
systemctl daemon-reload
systemctl start sync-bot
```

本地服务器容器 + 原生机器人、纯 Pod（quadlet）等更多托管方式见仓库 `server/` 与 `k8s/` 目录。

## 命令行

```
用法: telegram_sync_bot <COMMAND>

命令:
  run    运行机器人
         -d, --data <DIR>            数据目录
         -l, --localserver <URL>     本地服务器地址（如 http://127.0.0.1:8081）
         -c, --container-manager     容器管理器（podman 等）
         -i, --container-id          容器 ID / 名称
         -f, --fav-score-limit       收藏得分阈值（默认 10）
         -F, --dislike-score-limit   删除得分阈值（默认 -10）
  delete 按文件名删除数据目录中的文件、数据库记录及频道消息
```

`delete` 使用示例（清理所有图片/视频）：

```sh
fd ".*\.[jpg|mp4]" '/path/to/data' -X podman run --name bot -it --env-file .env \
    -v /path/to/data:/app/data --replace bot:0.X.0 delete -d /app/data {/}
```

## 开发

要求 **Rust 1.88+（2024 edition）**（代码使用了 let-chains）。

在 `.env` 中设置 `DATABASE_URL` 生成 entity crate：

```
DATABASE_URL=sqlite://data/data.db
```

```sh
cargo install sea-orm-cli
mkdir data
sea-orm-cli migrate refresh
sea-orm-cli generate entity --expanded-format -o bot/src/storage/entity/inner
```

## 项目结构

```
├── bot/                 # 机器人主程序（Rust crate）
│   ├── Containerfile    # bot 镜像构建文件
│   └── src/
│       ├── handler/     # 消息 / 命令 / 回调 / 表情回应处理
│       └── storage/     # 数据库、下载传输、文件归档
├── server/              # telegram-bot-api 本地服务器
│   ├── Containerfile    # server 镜像构建文件
│   └── start.sh
├── .github/workflows/   # CI：镜像构建推送（GHCR）、Release 发布
└── k8s/                 # Kubernetes 部署清单
```
