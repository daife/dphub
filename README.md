# dphub

Rust DeepSeek beta chat completions relay with SQLite quota accounting.

## 配置

编辑 `config.toml`：

- `deepseek.api_key`：你的 DeepSeek API Key
- `quota.id_daily_limit`：仅 androidid 用户每日额度，默认 `250000`
- `quota.verified_daily_limit`：androidid+手机号用户每日额度，默认 `1000000`
- `quota.referral_new_user_bonus`：被邀请注册的新手机号可存池奖励，默认 `250000`
- `quota.referral_inviter_bonus`：邀请码所属手机号可存池奖励，默认 `250000`
- `database.path`：SQLite 数据库路径，默认 `./data/dphub.sqlite`
- `admin.token`：管理员后台令牌，请部署前改成足够长的随机字符串

## 启动

```bash
cargo run --release
```

也可以指定配置文件：

```bash
DPHUB_CONFIG=/path/to/config.toml cargo run --release
```

## 服务器编译部署

以下示例假设服务器是 Linux，并且项目放在 `/root/dphub`。

1. 安装 Rust：

```bash
curl https://sh.rustup.rs -sSf | sh
source ~/.cargo/env
rustc --version
cargo --version
```

2. 拉取代码：

```bash
cd /root
git clone git@github.com:daife/dphub.git
cd /root/dphub
```

3. 修改配置：

```bash
cp /root/dphub/config.toml.example /root/dphub/config.toml
nano /root/dphub/config.toml
```

至少需要把 `deepseek.api_key` 改成真实 DeepSeek API Key。默认监听 `0.0.0.0:8000`，SQLite 数据库默认写入 `/root/dphub/data/dphub.sqlite`。
同时需要把 `admin.token` 改成只有你知道的随机字符串，用于访问管理员后台和后台 API。

4. 编译 release 二进制：

```bash
cd /root/dphub
cargo build --release
```

5. 手动验证启动：

```bash
cd /root/dphub
./target/release/dphub
```

另开一个终端验证：

```bash
curl http://YOUR_SERVER_IP:8000/health
```

返回 `ok` 说明服务正常。

6. 配置 systemd：

```bash
nano /etc/systemd/system/dphub.service
```

写入：

```ini
[Unit]
Description=dphub DeepSeek relay service
After=network.target

[Service]
Type=simple
WorkingDirectory=/root/dphub
ExecStart=/root/dphub/target/release/dphub
Environment=DPHUB_CONFIG=/root/dphub/config.toml
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

启动并设置开机自启：

```bash
systemctl daemon-reload
systemctl enable dphub
systemctl start dphub
systemctl status dphub
```

查看日志：

```bash
journalctl -u dphub -f
```

7. 放行端口：

```bash
ufw allow 8000/tcp
```

如果使用云服务器，还需要在云厂商安全组中放行 TCP `8000` 入站。

## 管理员后台

浏览器打开：

```text
http://YOUR_SERVER_IP:8000/admin
```

页面会要求填写 `config.toml` 中的 `admin.token`。Token 会保存到当前浏览器的 `localStorage`，后台 API 请求会自动携带：

```http
Authorization: Bearer <admin.token>
```

后台当前支持：

- 查看注册手机号总数。
- 查看所有手机号今日总 token 消耗。
- 查看所有手机号可存池总余额。
- 查看已达到手机号日额度的人数。
- 按手机号、邀请码、`user_id` 搜索。
- 按可存池余额、今日用量设置最小值和最大值筛选。
- 按是否达到日额度筛选。
- 按手机号、今日用量、可存池余额排序。
- 分页查看用户列表。
- 为已注册手机号发放可存池额度。

后台统计接口：

```bash
curl -X GET 'http://YOUR_SERVER_IP:8000/admin/api/overview?limit=50&offset=0&sort=used_desc' \
  -H 'Authorization: Bearer YOUR_ADMIN_TOKEN'
```

可选查询参数：

| 参数 | 说明 |
| --- | --- |
| `q` | 模糊搜索手机号、邀请码、`user_id` |
| `min_pool` / `max_pool` | 可存池余额范围 |
| `min_used` / `max_used` | 今日手机号 token 用量范围 |
| `over_daily_limit` | `true` 只看达到日额度用户，`false` 只看未达到日额度用户 |
| `sort` | `phone_asc`、`phone_desc`、`used_desc`、`used_asc`、`pool_desc`、`pool_asc` |
| `limit` | 每页数量，服务端限制在 `1` 到 `500` |
| `offset` | 分页偏移 |

响应示例：

```json
{
  "totals": {
    "registered_phone_count": 12,
    "total_pool_balance": 3000000,
    "today_phone_used_tokens": 450000,
    "over_daily_limit_count": 1
  },
  "filtered_count": 12,
  "limit": 50,
  "offset": 0,
  "users": [
    {
      "phone": "13800000000",
      "invite_code": "A1B2C3D4E5F6",
      "user_id": "u_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
      "pool_balance": 250000,
      "today_used_tokens": 12000,
      "daily_limit": 1000000,
      "over_daily_limit": false
    }
  ]
}
```

后台发放可存池额度：

```bash
curl -X POST http://YOUR_SERVER_IP:8000/admin/api/pool/grant \
  -H 'Authorization: Bearer YOUR_ADMIN_TOKEN' \
  -H 'Content-Type: application/json' \
  -d '{"phone":"13800000000","amount":250000}'
```

响应：

```json
{
  "phone": "13800000000",
  "pool_balance": 500000
}
```

发放接口只接受正数 `amount`，且手机号必须已经注册。手机号不存在时返回 `404`，不会自动创建账户。

## 请求

```bash
curl -X POST http://YOUR_SERVER_IP:8000/v1/beta/chat/completions \
  -H 'Authorization: Bearer androidid-or-androidid-phone' \
  -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hi"}],"stream":false}'
```

`stream=true` 会返回 `400`，因为服务需要从非流式 JSON 响应中读取 `usage.total_tokens` 来计费。

转发给 DeepSeek 官方时，服务会自动在请求体中加入 `user_id`：

- `Authorization: Bearer androidid` 使用该 androidid 对应的唯一 `user_id`
- `Authorization: Bearer androidid-phone` 使用该手机号对应的唯一 `user_id`

`user_id` 由服务生成，不包含 androidid、手机号等隐私信息。

并发处理规则：

- 不同 `androidid` / 手机号用户可以并行请求。
- 同一个 `androidid` 或手机号也允许并发请求。
- 额度检查发生在调用 DeepSeek 之前，实际 token 消耗在官方返回后累加；因此并发请求可能同时通过额度检查，最后一次或多次请求可能让总消耗超过配置阈值。
- token 累加和可存池扣减使用 SQLite 持久化更新。

## 手机号注册和邀请码

注册接口：

```bash
curl -X POST http://YOUR_SERVER_IP:8000/v1/register \
  -H 'Content-Type: application/json' \
  -d '{"phone":"13800000000","invite_code":"可为空"}'
```

`invite_code` 可以省略、为 `null` 或空字符串。响应：

```json
{
  "phone": "13800000000",
  "invite_code": "A1B2C3D4E5F6",
  "user_id": "u_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
  "pool_balance": 250000
}
```

如果手机号已存在，返回 `409`；如果邀请码不存在，返回 `400`。手机号首次自动建档时会生成唯一邀请码。

## 邀请码查询

安卓端使用带手机号的 `Authorization` 查询当前手机号的邀请码，不需要请求体：

```bash
curl -X GET http://YOUR_SERVER_IP:8000/v1/invite-code \
  -H 'Authorization: Bearer androidid-phone'
```

响应：

```json
{
  "phone": "13800000000",
  "invite_code": "A1B2C3D4E5F6"
}
```

注意：

- 必须使用 `Authorization: Bearer androidid-phone`。
- 如果只传 `Authorization: Bearer androidid`，返回 `400`。
- 如果该手机号还没有账户记录，服务会自动为该手机号建档并生成邀请码；不会发放邀请奖励。

## 额度查询

安卓端使用和 chat 接口相同的 `Authorization` 查询额度，不需要请求体：

```bash
curl -X GET http://YOUR_SERVER_IP:8000/v1/quota \
  -H 'Authorization: Bearer androidid-or-androidid-phone'
```

仅 androidid 的响应：

```json
{
  "used_tokens": 125000,
  "daily_limit": 250000,
  "usage_ratio": 0.5
}
```

androidid+手机号的响应：

```json
{
  "used_tokens": 500000,
  "daily_limit": 1000000,
  "usage_ratio": 0.5,
  "pool_balance": 250000
}
```

字段说明：

- `used_tokens`：今日已用 token。`androidid-phone` 会取 androidid 和手机号今日消耗中更高的那个，并同步二者。
- `daily_limit`：本次身份类型对应的每日额度。纯 androidid 默认 `250000`；androidid+手机号默认 `1000000`。
- `usage_ratio`：`used_tokens / daily_limit`，范围固定为 `0.0` 到 `1.0`；超过每日额度时返回 `1.0`。
- `pool_balance`：仅 androidid+手机号返回，表示手机号可存池剩余 token 数，直接显示该数值即可。

额度查询错误：

- `401`：缺少 `Authorization` 或格式不是 `Bearer ...`。
- `500`：SQLite 读写失败。

## 安卓端错误处理

DeepSeek 官方接口有响应时，服务会原样返回官方 HTTP 状态码、`Content-Type` 和响应 body，安卓端可按 DeepSeek 官方错误格式处理。

服务自身错误统一返回 JSON：

```json
{
  "error": "错误说明"
}
```

常见错误：

| HTTP 状态码 | `error` | 场景 |
| --- | --- | --- |
| `400` | `stream=true is not supported because usage.total_tokens must be recorded` | chat 请求启用了流式响应 |
| `400` | `request body must be valid JSON` | chat 请求体不是合法 JSON |
| `400` | `request body must be a JSON object` | chat 请求体不是 JSON object |
| `400` | `invite code does not exist` | 注册时邀请码不存在；不会创建手机号账户，也不会发放奖励 |
| `400` | `phone is required to query invite code` | 查询邀请码时只传了 androidid，没有传手机号 |
| `400` | `invalid admin request` | 管理后台发放额度时手机号为空或 `amount <= 0` |
| `401` | `missing authorization header` | chat 请求缺少 `Authorization` |
| `401` | `authorization header must be Bearer token` | chat 请求 `Authorization` 不是 `Bearer ...` 格式 |
| `401` | `admin authorization required` | 管理后台 API 缺少管理员 Token 或 Token 不正确 |
| `404` | `phone account does not exist` | 管理后台发放额度的手机号尚未注册 |
| `409` | `phone already registered` | 注册手机号已存在，或手机号为空 |
| `429` | `quota exceeded` | 今日额度不足且手机号可存池无余额 |
| `500` | `quota database error` | SQLite 读写失败 |
| `502` | `failed to call upstream` | 无法连接 DeepSeek 官方接口 |
| `502` | `failed to read upstream response` | 读取 DeepSeek 官方响应失败 |

注册接口会先校验手机号是否已存在和邀请码是否有效，再创建账户。邀请码错误时不会写入 `phone_account`，也不会增加任何可存池额度。

## 手机号可存池

手机号可存池保存在 SQLite 的 `phone_pool.balance_tokens`。首次出现的手机号会自动创建，默认余额为 `0`。需要充值时可直接更新数据库：

```sql
INSERT INTO phone_pool (phone, balance_tokens)
VALUES ('13800000000', 100000)
ON CONFLICT(phone) DO UPDATE SET balance_tokens = balance_tokens + 100000;
```
