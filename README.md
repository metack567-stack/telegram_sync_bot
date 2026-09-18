# 关于

这是一个 Telegram 机器人（bot），用于下载所有者转发给它的文件。

使用 Rust 和 Teloxide 构建。

## 直接发送给机器人的文件

机器人会下载文件并保存到指定目录。

所有者可以对消息添加表情回应来管理文件：
- "👍" | "❤"：收藏，并将文件移动到收藏目录
- "👎"：将文件移动到回收站

## 发送到机器人托管频道的文件

最初，机器人所有者发送 `/toggle <bypasskey>` 给机器人，在以下状态间切换：
- `paused`：暂停机器人
- `active`：同步文件并响应表情回应
- `partially active`：响应表情回应但不同步文件

（`<bypasskey>` 可以在日志中看到，发送 `/bypasskey` 可以重新在日志中打印该密码）

机器人会对文件消息设置 "🫡" 表情，表示文件正在下载。

下载完成后，机器人会设置 "👌"。（失败为 "😭"，取消为 "😨"，内部错误为 "👾"）

人们可以用表情回应文件，机器人会统计文件的得分。

| 表情 | 得分 |
| --- | --- |
|👍😁🙏😇🤗|+1|
|❤🔥🥰🎉🍌💋💘😘|+2|
|👎🤯😱😢🥴🌚😐🖕😨|-1|
|🤬🤮💩🤡💔😡|-2|

如果得分 >= fav_score_limit，机器人会将文件硬链接到收藏目录并置顶。

如果得分 < delete_score_limit，机器人会将文件硬链接到回收站并从频道删除。

否则，机器人会将文件硬链接到 normal 目录，并在必要时取消置顶。

注意：获取 ReactionCountUpdate 需要几分钟，因此机器人不会立即处理频道的表情回应。

# 部署

你可以创建包含以下内容的 `.env` 文件：

```
# 从 BotFather 获取
TELOXIDE_TOKEN=xxxxxxxxxx:xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
# 你的 Telegram ID，在个人资料中可见
BYPASS_USERS=xxxxx,xxxxx
# 如果你想使用本地服务器：
TELEGRAM_API_ID=...
TELEGRAM_API_HASH=...
```

部署方式：
## 文件大小限制 20MB

```
一个将文件同步到本地服务器的 Telegram 机器人。

用法: telegram_sync_bot <COMMAND>

命令:
  run    运行机器人
  delete 按 file_name 删除 data 目录中的文件，并删除数据库中的记录，同时删除频道中的消息。数据库不能被其他进程锁定，也不应存在其他机器人实例。
```

```sh
telegram_sync_bot run -d /path/to/data
```

## 无文件大小限制（本地服务器）

你需要先从 [Telegram](https://core.telegram.org/obtaining_api_id) 申请 telegram api id 和 hash。
（如果申请时总是出现 `Error`，可以尝试用 `cloudflare warp` 作为 VPN）

以下所有方法都先以容器方式运行本地服务器。

（你也可以原生运行本地服务器，只需在启动 `telegram_sync_bot` 时省略 `-c` 和 `-i` 参数。
这里不再赘述）

先获取本地服务器镜像：

准备（仅限 Windows 和 MacOS，使用 podman）：
```sh
podman machine init -v /path/to/output:/path/to/output bot_machine
podman machine start bot_machine
```

你可以使用以下命令构建 telegram api bot 本地服务器镜像：
```sh
podman build --target server -t server --network host server
```
或者从 Release 页面下载并加载（`server.tar.gz`），我已经通过 GitHub Action 构建好了。

这里提供 4 种方式：
- native（原生）
- pod
- podman kube play
- k8s

### 常规方式：服务器在容器中，机器人原生运行

```sh
podman run --name server -itd --env-file .env -p 8081:8081 server

telegram_sync_bot run -d /path/to/output -l http://127.0.0.1:8081 -c podman -i server
```

### 以 pod 方式运行

将 `telegram_sync_bot` 构建为容器镜像：
```sh
# 构建 bot 镜像
podman build --target bot -t bot:$(cargo pkgid -p telegram_sync_bot | sed -n "s/.*@//p") --network host bot
```
或者从 Release 页面下载并加载（bot.tar.gz）。

在 pod 中启动服务器和机器人：
```sh
podman pod create sync_bot

podman run --pod sync_bot --name server -itd --env-file .env \
    -v /path/to/data:/app/data server
podman run --pod sync_bot --name bot -itd --env-file .env --stop-signal SIGINT \
    -v /path/to/data:/app/data  \
    bot:0.X.0 \
    run -d /app/data -l http://server:8081
```

### 使用 podman kube play 运行

根据需要修改 `sync-bot.yaml`。

你可以先从 Release 页面下载并加载 `server.tar.gz` 和 `bot.tar.gz`。
或者使用以下命令自动构建镜像（会花费大量时间）。
```sh
podman kube play sync-bot.yaml
```

### 使用 k8s 运行

先构建并保存镜像为 `.tar.gz`，或从 Release 页面下载。

根据需要修改 `.env` 和 `k8s/pv.yaml` 等文件。
```sh
# 加载本地镜像
sudo ctr -n=k8s.io images import /tmp/server.tar.gz
sudo ctr -n=k8s.io images import /tmp/bot.tar.gz

sudo crictl image
# 你应该能看到 localhost/bot 和 localhost/server

kubectl apply -k .
```

# Docker 部署

## 使用 docker compose（推荐）

创建 `docker-compose.yml`：

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
    stop_signal: SIGINT
    command: run -d /app/data -l http://server:8081
```

`.env` 中需要额外配置：

```
TELEGRAM_DATA_DIR=/path/to/data
APP_DATA_DIR=/path/to/app-data
```

启动：

```sh
docker compose up -d
```

更新到最新镜像：

```sh
docker compose pull && docker compose up -d
```

## 使用 docker run

先构建镜像（或从 GHCR 拉取 `ghcr.io/metack567-stack/telegram_sync_bot/bot:dev` / `server:dev`）：

```sh
docker build -f bot/Containerfile --target bot -t bot:0.X.0 bot
docker build -f server/Containerfile --target server -t server:latest server
```

创建网络并启动服务器：

```sh
docker network create tgsync
docker run --name server --network tgsync -itd --env-file .env -p 8081:8081 \
    -v /path/to/data:/app/data server
```

启动机器人：

```sh
docker run --name bot --network tgsync -itd --env-file .env --stop-signal SIGINT \
    -v /path/to/data:/app/data -v /path/to/db:/app/db \
    bot:0.X.0 run -d /app/data -l http://server:8081
```

# Systemd 服务
## 原生运行（无本地服务器）：
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
## 或使用本地服务器容器 + 原生 telegram_sync_bot（首次设置后）：
```ini
# /etc/systemd/system/sync-bot.service
[Unit]
Description=Telegram file sync bot
After=network-online.target

[Service]
Type=simple
User=<...>
WorkingDirectory=</path/to/output>
ExecStartPre=/usr/bin/podman restart server
ExecStart=/usr/local/bin/telegram_sync_bot run -l http://127.0.0.1:8081 -c podman -i server
ExecStop=/bin/bash -c 'kill -SIGINT $MAINPID; for i in {1..5}; do sleep 1; kill -0 $MAINPID 2>/dev/null || exit 0; done; kill -SIGKILL $MAINPID'
ExecStopPost=/usr/bin/podman stop server
Restart=on-failure
Environment="TELOXIDE_TOKEN=<...>"
Environment="BYPASS_USERS=<...>"

[Install]
WantedBy=multi-user.target
```
## 或纯 pod 方式（构建或加载镜像后）：
```ini
# /etc/container/systemd/users/<UserID>/sync-bot.kube
[Unit]
Description=Telegram file sync bot
After=network-online.target

[Kube]
Yaml=/etc/containers/systemd/users/<UserID>/sync-bot.yaml

[Install]
WantedBy=default.target
```
关于将 podman kube play 作为 systemd 服务使用，请搜索 `podman quadlet`。

```sh
systemctl --user daemon-reload
systemctl start --user sync-bot
```

注意：你可以使用 `/usr/lib/systemd/system-generators/podman-system-generator --user --dryrun` 检查生成的服务文件。

# 提示

使用 `fd` 删除数据库、频道消息和 data 目录中的文件：

```sh
fd ".*\.[jpg|mp4]" '/path/to/data' -X podman run --name bot -it --env-file .env -v /path/to/data:/app/data --replace bot:0.X.0 delete -d /app/data {/}
```

# 开发

**Rust 2024 是必需的**

在 `.env` 中设置 `DATABASE_URL` 以生成 entity crate。

```
# .env
DATABASE_URL=sqlite://data/data.db
```

然后你可以运行以下命令创建数据库并生成 entity：

```sh
cargo install sea-orm-cli
mkdir data
sea-orm-cli migrate refresh
sea-orm-cli generate entity --expanded-format -o bot/src/storage/entity/inner
```
