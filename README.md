# dphub

Rust DeepSeek beta chat completions relay with SQLite quota accounting.

## 配置

编辑 `config.toml`：

- `deepseek.api_key`：你的 DeepSeek API Key
- `quota.id_daily_limit`：仅 androidid 用户每日额度，默认 `250000`
- `quota.verified_daily_limit`：androidid+手机号用户每日额度，默认 `1000000`
- `database.path`：SQLite 数据库路径，默认 `./data/dphub.sqlite`

## 启动

```bash
cargo run --release
```

也可以指定配置文件：

```bash
DPHUB_CONFIG=/path/to/config.toml cargo run --release
```

## 请求

```bash
curl -X POST http://YOUR_SERVER_IP:8000/v1/beta/chat/completions \
  -H 'Authorization: Bearer androidid-or-androidid-phone' \
  -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hi"}],"stream":false}'
```

`stream=true` 会返回 `400`，因为服务需要从非流式 JSON 响应中读取 `usage.total_tokens` 来计费。

## 手机号可存池

手机号可存池保存在 SQLite 的 `phone_pool.balance_tokens`。首次出现的手机号会自动创建，默认余额为 `0`。需要充值时可直接更新数据库：

```sql
INSERT INTO phone_pool (phone, balance_tokens)
VALUES ('13800000000', 100000)
ON CONFLICT(phone) DO UPDATE SET balance_tokens = balance_tokens + 100000;
```
