use chrono::{Local, NaiveDate};
use serde_json::Value;
use sqlx::{sqlite::SqlitePoolOptions, SqliteConnection, SqlitePool};
use thiserror::Error;
use uuid::Uuid;

use crate::config::QuotaConfig;

#[derive(Clone)]
pub struct QuotaStore {
    pool: SqlitePool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    IdOnly { id: String },
    IdAndPhone { id: String, phone: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeTarget {
    IdOnly,
    VerifiedDaily,
    PhonePool,
}

#[derive(Debug, Error)]
pub enum QuotaError {
    #[error("missing authorization header")]
    MissingAuthorization,
    #[error("authorization header must be Bearer token")]
    InvalidAuthorization,
    #[error("quota exceeded")]
    QuotaExceeded,
    #[error("phone already registered")]
    PhoneAlreadyRegistered,
    #[error("invite code does not exist")]
    InvalidInviteCode,
    #[error("phone is required to query invite code")]
    PhoneRequired,
    #[error("database error")]
    Database(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, QuotaError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationResult {
    pub phone: String,
    pub invite_code: String,
    pub user_id: String,
    pub pool_balance: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaStatus {
    pub used_tokens: i64,
    pub daily_limit: i64,
    pub usage_ratio: f64,
    pub pool_balance: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCodeInfo {
    pub phone: String,
    pub invite_code: String,
}

impl QuotaStore {
    pub async fn connect(path: &std::path::Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let database_url = format!("sqlite://{}?mode=rwc", path.display());
        let pool = SqlitePoolOptions::new()
            .max_connections(10)
            .connect(&database_url)
            .await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    #[cfg(test)]
    pub async fn connect_memory() -> anyhow::Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS daily_usage (
                subject_type TEXT NOT NULL,
                subject_value TEXT NOT NULL,
                usage_date TEXT NOT NULL,
                total_tokens INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (subject_type, subject_value, usage_date)
            );
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS id_account (
                android_id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL UNIQUE
            );
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS phone_account (
                phone TEXT PRIMARY KEY,
                invite_code TEXT NOT NULL UNIQUE,
                user_id TEXT NOT NULL UNIQUE
            );
            "#,
        )
        .execute(&self.pool)
        .await?;

        self.migrate_phone_account_user_id().await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS phone_pool (
                phone TEXT PRIMARY KEY,
                balance_tokens INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn migrate_phone_account_user_id(&self) -> Result<()> {
        let columns: Vec<(String,)> = sqlx::query_as("SELECT name FROM pragma_table_info('phone_account')")
            .fetch_all(&self.pool)
            .await?;
        if columns.iter().any(|column| column.0 == "user_id") {
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;
        sqlx::query("ALTER TABLE phone_account RENAME TO phone_account_old")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r#"
            CREATE TABLE phone_account (
                phone TEXT PRIMARY KEY,
                invite_code TEXT NOT NULL UNIQUE,
                user_id TEXT NOT NULL UNIQUE
            );
            "#,
        )
        .execute(&mut *tx)
        .await?;

        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT phone, invite_code FROM phone_account_old")
                .fetch_all(&mut *tx)
                .await?;
        for (phone, invite_code) in rows {
            loop {
                let user_id = new_user_id();
                let result = sqlx::query(
                    "INSERT INTO phone_account (phone, invite_code, user_id) VALUES (?1, ?2, ?3)",
                )
                .bind(&phone)
                .bind(&invite_code)
                .bind(&user_id)
                .execute(&mut *tx)
                .await;
                match result {
                    Ok(_) => break,
                    Err(sqlx::Error::Database(err)) if err.is_unique_violation() => continue,
                    Err(err) => return Err(err.into()),
                }
            }
        }

        sqlx::query("DROP TABLE phone_account_old")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn reserve(&self, principal: &Principal, config: &QuotaConfig) -> Result<ChargeTarget> {
        self.reserve_for_date(principal, config, today()).await
    }

    async fn reserve_for_date(
        &self,
        principal: &Principal,
        config: &QuotaConfig,
        date: NaiveDate,
    ) -> Result<ChargeTarget> {
        let mut tx = self.pool.begin().await?;

        let target = match principal {
            Principal::IdOnly { id } => {
                ensure_id_account_row(&mut tx, id).await?;
                ensure_daily_row(&mut tx, "id", id, date).await?;
                let id_used = get_daily_usage(&mut tx, "id", id, date).await?;
                if id_used < config.id_daily_limit {
                    ChargeTarget::IdOnly
                } else {
                    return Err(QuotaError::QuotaExceeded);
                }
            }
            Principal::IdAndPhone { id, phone } => {
                ensure_id_account_row(&mut tx, id).await?;
                ensure_phone_account_row(&mut tx, phone).await?;
                ensure_daily_row(&mut tx, "id", id, date).await?;
                ensure_daily_row(&mut tx, "phone", phone, date).await?;
                ensure_phone_pool_row(&mut tx, phone).await?;

                let id_used = get_daily_usage(&mut tx, "id", id, date).await?;
                let phone_used = get_daily_usage(&mut tx, "phone", phone, date).await?;
                let unified = id_used.max(phone_used);
                if id_used != unified {
                    set_daily_usage(&mut tx, "id", id, date, unified).await?;
                }
                if phone_used != unified {
                    set_daily_usage(&mut tx, "phone", phone, date, unified).await?;
                }

                if unified < config.verified_daily_limit {
                    ChargeTarget::VerifiedDaily
                } else if get_phone_pool(&mut tx, phone).await? > 0 {
                    ChargeTarget::PhonePool
                } else {
                    return Err(QuotaError::QuotaExceeded);
                }
            }
        };

        tx.commit().await?;
        Ok(target)
    }

    pub async fn register_phone(
        &self,
        phone: &str,
        invite_code: Option<&str>,
        config: &QuotaConfig,
    ) -> Result<RegistrationResult> {
        let phone = phone.trim();
        if phone.is_empty() {
            return Err(QuotaError::PhoneAlreadyRegistered);
        }

        let invite_code = invite_code
            .map(str::trim)
            .filter(|code| !code.is_empty());

        let mut tx = self.pool.begin().await?;

        if phone_account_exists(&mut tx, phone).await? {
            return Err(QuotaError::PhoneAlreadyRegistered);
        }

        let inviter_phone = match invite_code {
            Some(code) => Some(
                get_phone_by_invite_code(&mut tx, code)
                    .await?
                    .ok_or(QuotaError::InvalidInviteCode)?,
            ),
            None => None,
        };

        let (new_invite_code, user_id) = insert_phone_account_with_new_invite(&mut tx, phone).await?;
        ensure_phone_pool_row(&mut tx, phone).await?;

        if let Some(inviter_phone) = inviter_phone.as_deref() {
            ensure_phone_pool_row(&mut tx, inviter_phone).await?;
            add_phone_pool(&mut tx, phone, config.referral_new_user_bonus).await?;
            add_phone_pool(&mut tx, inviter_phone, config.referral_inviter_bonus).await?;
        }

        let pool_balance = get_phone_pool(&mut tx, phone).await?;
        tx.commit().await?;

        Ok(RegistrationResult {
            phone: phone.to_owned(),
            invite_code: new_invite_code,
            user_id,
            pool_balance,
        })
    }

    pub async fn user_id_for_principal(&self, principal: &Principal) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let user_id = match principal {
            Principal::IdOnly { id } => {
                ensure_id_account_row(&mut tx, id).await?;
                get_id_user_id(&mut tx, id).await?
            }
            Principal::IdAndPhone { phone, .. } => {
                ensure_phone_account_row(&mut tx, phone).await?;
                get_phone_user_id(&mut tx, phone).await?
            }
        };
        tx.commit().await?;
        Ok(user_id)
    }

    pub async fn quota_status(
        &self,
        principal: &Principal,
        config: &QuotaConfig,
    ) -> Result<QuotaStatus> {
        self.quota_status_for_date(principal, config, today()).await
    }

    pub async fn invite_code_for_principal(&self, principal: &Principal) -> Result<InviteCodeInfo> {
        let Principal::IdAndPhone { phone, .. } = principal else {
            return Err(QuotaError::PhoneRequired);
        };

        let mut tx = self.pool.begin().await?;
        ensure_phone_account_row(&mut tx, phone).await?;
        let invite_code = get_phone_invite_code(&mut tx, phone).await?;
        tx.commit().await?;

        Ok(InviteCodeInfo {
            phone: phone.to_owned(),
            invite_code,
        })
    }

    async fn quota_status_for_date(
        &self,
        principal: &Principal,
        config: &QuotaConfig,
        date: NaiveDate,
    ) -> Result<QuotaStatus> {
        let mut tx = self.pool.begin().await?;

        let status = match principal {
            Principal::IdOnly { id } => {
                ensure_id_account_row(&mut tx, id).await?;
                ensure_daily_row(&mut tx, "id", id, date).await?;
                let used_tokens = get_daily_usage(&mut tx, "id", id, date).await?;
                QuotaStatus {
                    used_tokens,
                    daily_limit: config.id_daily_limit,
                    usage_ratio: usage_ratio(used_tokens, config.id_daily_limit),
                    pool_balance: None,
                }
            }
            Principal::IdAndPhone { id, phone } => {
                ensure_id_account_row(&mut tx, id).await?;
                ensure_phone_account_row(&mut tx, phone).await?;
                ensure_daily_row(&mut tx, "id", id, date).await?;
                ensure_daily_row(&mut tx, "phone", phone, date).await?;
                ensure_phone_pool_row(&mut tx, phone).await?;

                let id_used = get_daily_usage(&mut tx, "id", id, date).await?;
                let phone_used = get_daily_usage(&mut tx, "phone", phone, date).await?;
                let used_tokens = id_used.max(phone_used);
                if id_used != used_tokens {
                    set_daily_usage(&mut tx, "id", id, date, used_tokens).await?;
                }
                if phone_used != used_tokens {
                    set_daily_usage(&mut tx, "phone", phone, date, used_tokens).await?;
                }

                QuotaStatus {
                    used_tokens,
                    daily_limit: config.verified_daily_limit,
                    usage_ratio: usage_ratio(used_tokens, config.verified_daily_limit),
                    pool_balance: Some(get_phone_pool(&mut tx, phone).await?),
                }
            }
        };

        tx.commit().await?;
        Ok(status)
    }

    pub async fn charge(
        &self,
        principal: &Principal,
        target: ChargeTarget,
        total_tokens: i64,
    ) -> Result<()> {
        self.charge_for_date(principal, target, total_tokens, today()).await
    }

    async fn charge_for_date(
        &self,
        principal: &Principal,
        target: ChargeTarget,
        total_tokens: i64,
        date: NaiveDate,
    ) -> Result<()> {
        if total_tokens <= 0 {
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;
        match (principal, target) {
            (Principal::IdOnly { id }, ChargeTarget::IdOnly) => {
                add_daily_usage(&mut tx, "id", id, date, total_tokens).await?;
            }
            (Principal::IdAndPhone { id, phone }, ChargeTarget::VerifiedDaily) => {
                add_daily_usage(&mut tx, "id", id, date, total_tokens).await?;
                add_daily_usage(&mut tx, "phone", phone, date, total_tokens).await?;
            }
            (Principal::IdAndPhone { phone, .. }, ChargeTarget::PhonePool) => {
                sqlx::query(
                    r#"
                    UPDATE phone_pool
                    SET balance_tokens = MAX(balance_tokens - ?1, 0)
                    WHERE phone = ?2
                    "#,
                )
                .bind(total_tokens)
                .bind(phone)
                .execute(&mut *tx)
                .await?;
            }
            _ => {}
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn clear_today_usage(&self) -> Result<()> {
        sqlx::query("DELETE FROM daily_usage WHERE usage_date <= ?1")
            .bind(today().to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[cfg(test)]
    async fn set_phone_pool(&self, phone: &str, balance: i64) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO phone_pool (phone, balance_tokens)
            VALUES (?1, ?2)
            ON CONFLICT(phone) DO UPDATE SET balance_tokens = excluded.balance_tokens
            "#,
        )
        .bind(phone)
        .bind(balance)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    #[cfg(test)]
    async fn daily_usage(&self, subject_type: &str, value: &str, date: NaiveDate) -> Result<i64> {
        let mut conn = self.pool.acquire().await?;
        get_daily_usage(&mut conn, subject_type, value, date).await
    }

    #[cfg(test)]
    async fn pool_balance(&self, phone: &str) -> Result<i64> {
        let mut conn = self.pool.acquire().await?;
        get_phone_pool(&mut conn, phone).await
    }
}

pub fn parse_authorization(value: Option<&str>) -> Result<Principal> {
    let value = value.ok_or(QuotaError::MissingAuthorization)?;
    let token = value
        .strip_prefix("Bearer ")
        .ok_or(QuotaError::InvalidAuthorization)?
        .trim();

    if token.is_empty() {
        return Err(QuotaError::InvalidAuthorization);
    }

    match token.split_once('-') {
        Some((id, phone)) if !id.trim().is_empty() && !phone.trim().is_empty() => {
            Ok(Principal::IdAndPhone {
                id: id.trim().to_owned(),
                phone: phone.trim().to_owned(),
            })
        }
        Some(_) => Err(QuotaError::InvalidAuthorization),
        None => Ok(Principal::IdOnly {
            id: token.to_owned(),
        }),
    }
}

pub fn extract_total_tokens(body: &[u8]) -> Option<i64> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value
        .get("usage")?
        .get("total_tokens")?
        .as_i64()
        .filter(|tokens| *tokens > 0)
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

fn new_invite_code() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_ascii_uppercase()
}

fn new_user_id() -> String {
    format!("u_{}", Uuid::new_v4().simple())
}

fn usage_ratio(used_tokens: i64, daily_limit: i64) -> f64 {
    if daily_limit <= 0 {
        return 1.0;
    }
    ((used_tokens as f64) / (daily_limit as f64)).clamp(0.0, 1.0)
}

async fn ensure_daily_row(
    conn: &mut SqliteConnection,
    subject_type: &str,
    value: &str,
    date: NaiveDate,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT OR IGNORE INTO daily_usage (subject_type, subject_value, usage_date, total_tokens)
        VALUES (?1, ?2, ?3, 0)
        "#,
    )
    .bind(subject_type)
    .bind(value)
    .bind(date.to_string())
    .execute(conn)
    .await?;
    Ok(())
}

async fn ensure_phone_pool_row(
    conn: &mut SqliteConnection,
    phone: &str,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO phone_pool (phone, balance_tokens) VALUES (?1, 0)")
        .bind(phone)
        .execute(conn)
        .await?;
    Ok(())
}

async fn ensure_phone_account_row(
    conn: &mut SqliteConnection,
    phone: &str,
) -> std::result::Result<(), sqlx::Error> {
    if phone_account_exists(conn, phone).await? {
        return Ok(());
    }
    let (_invite_code, _user_id) = insert_phone_account_with_new_invite(conn, phone).await?;
    Ok(())
}

async fn ensure_id_account_row(
    conn: &mut SqliteConnection,
    android_id: &str,
) -> std::result::Result<(), sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(1) FROM id_account WHERE android_id = ?1")
        .bind(android_id)
        .fetch_one(&mut *conn)
        .await?;
    if row.0 > 0 {
        return Ok(());
    }

    for _ in 0..10 {
        let user_id = new_user_id();
        let result = sqlx::query("INSERT INTO id_account (android_id, user_id) VALUES (?1, ?2)")
            .bind(android_id)
            .bind(&user_id)
            .execute(&mut *conn)
            .await;

        match result {
            Ok(_) => return Ok(()),
            Err(sqlx::Error::Database(err)) if err.is_unique_violation() => continue,
            Err(err) => return Err(err),
        }
    }

    Err(sqlx::Error::Protocol(
        "failed to generate a unique user_id".to_owned(),
    ))
}

async fn phone_account_exists(
    conn: &mut SqliteConnection,
    phone: &str,
) -> std::result::Result<bool, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(1) FROM phone_account WHERE phone = ?1")
        .bind(phone)
        .fetch_one(conn)
        .await?;
    Ok(row.0 > 0)
}

async fn get_phone_by_invite_code(
    conn: &mut SqliteConnection,
    invite_code: &str,
) -> std::result::Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT phone FROM phone_account WHERE invite_code = ?1")
            .bind(invite_code)
            .fetch_optional(conn)
            .await?;
    Ok(row.map(|row| row.0))
}

async fn insert_phone_account_with_new_invite(
    conn: &mut SqliteConnection,
    phone: &str,
) -> std::result::Result<(String, String), sqlx::Error> {
    for _ in 0..10 {
        let invite_code = new_invite_code();
        let user_id = new_user_id();
        let result = sqlx::query(
            "INSERT INTO phone_account (phone, invite_code, user_id) VALUES (?1, ?2, ?3)",
        )
                .bind(phone)
                .bind(&invite_code)
                .bind(&user_id)
                .execute(&mut *conn)
                .await;

        match result {
            Ok(_) => return Ok((invite_code, user_id)),
            Err(sqlx::Error::Database(err)) if err.is_unique_violation() => continue,
            Err(err) => return Err(err),
        }
    }

    Err(sqlx::Error::Protocol(
        "failed to generate a unique invite code".to_owned(),
    ))
}

async fn get_id_user_id(
    conn: &mut SqliteConnection,
    android_id: &str,
) -> std::result::Result<String, sqlx::Error> {
    let row: (String,) = sqlx::query_as("SELECT user_id FROM id_account WHERE android_id = ?1")
        .bind(android_id)
        .fetch_one(conn)
        .await?;
    Ok(row.0)
}

async fn get_phone_user_id(
    conn: &mut SqliteConnection,
    phone: &str,
) -> std::result::Result<String, sqlx::Error> {
    let row: (String,) = sqlx::query_as("SELECT user_id FROM phone_account WHERE phone = ?1")
        .bind(phone)
        .fetch_one(conn)
        .await?;
    Ok(row.0)
}

async fn get_phone_invite_code(
    conn: &mut SqliteConnection,
    phone: &str,
) -> std::result::Result<String, sqlx::Error> {
    let row: (String,) =
        sqlx::query_as("SELECT invite_code FROM phone_account WHERE phone = ?1")
            .bind(phone)
            .fetch_one(conn)
            .await?;
    Ok(row.0)
}

async fn add_phone_pool(
    conn: &mut SqliteConnection,
    phone: &str,
    amount: i64,
) -> std::result::Result<(), sqlx::Error> {
    if amount <= 0 {
        return Ok(());
    }

    sqlx::query(
        r#"
        INSERT INTO phone_pool (phone, balance_tokens)
        VALUES (?1, ?2)
        ON CONFLICT(phone)
        DO UPDATE SET balance_tokens = phone_pool.balance_tokens + excluded.balance_tokens
        "#,
    )
    .bind(phone)
    .bind(amount)
    .execute(conn)
    .await?;
    Ok(())
}

async fn get_daily_usage(
    conn: &mut SqliteConnection,
    subject_type: &str,
    value: &str,
    date: NaiveDate,
) -> std::result::Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        r#"
        SELECT total_tokens FROM daily_usage
        WHERE subject_type = ?1 AND subject_value = ?2 AND usage_date = ?3
        "#,
    )
    .bind(subject_type)
    .bind(value)
    .bind(date.to_string())
    .fetch_optional(conn)
    .await?
    .unwrap_or((0,));
    Ok(row.0)
}

async fn set_daily_usage(
    conn: &mut SqliteConnection,
    subject_type: &str,
    value: &str,
    date: NaiveDate,
    total_tokens: i64,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO daily_usage (subject_type, subject_value, usage_date, total_tokens)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(subject_type, subject_value, usage_date)
        DO UPDATE SET total_tokens = excluded.total_tokens
        "#,
    )
    .bind(subject_type)
    .bind(value)
    .bind(date.to_string())
    .bind(total_tokens)
    .execute(conn)
    .await?;
    Ok(())
}

async fn add_daily_usage(
    conn: &mut SqliteConnection,
    subject_type: &str,
    value: &str,
    date: NaiveDate,
    total_tokens: i64,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO daily_usage (subject_type, subject_value, usage_date, total_tokens)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(subject_type, subject_value, usage_date)
        DO UPDATE SET total_tokens = daily_usage.total_tokens + excluded.total_tokens
        "#,
    )
    .bind(subject_type)
    .bind(value)
    .bind(date.to_string())
    .bind(total_tokens)
    .execute(conn)
    .await?;
    Ok(())
}

async fn get_phone_pool(
    conn: &mut SqliteConnection,
    phone: &str,
) -> std::result::Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT balance_tokens FROM phone_pool WHERE phone = ?1")
        .bind(phone)
        .fetch_optional(conn)
        .await?
        .unwrap_or((0,));
    Ok(row.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota() -> QuotaConfig {
        QuotaConfig {
            id_daily_limit: 10,
            verified_daily_limit: 20,
            referral_new_user_bonus: 250000,
            referral_inviter_bonus: 250000,
        }
    }

    #[test]
    fn parses_authorization() {
        assert_eq!(
            parse_authorization(Some("Bearer android")).unwrap(),
            Principal::IdOnly {
                id: "android".into()
            }
        );
        assert_eq!(
            parse_authorization(Some("Bearer android-13800000000")).unwrap(),
            Principal::IdAndPhone {
                id: "android".into(),
                phone: "13800000000".into()
            }
        );
        assert!(parse_authorization(Some("Basic android")).is_err());
    }

    #[test]
    fn extracts_total_tokens() {
        let body = br#"{"usage":{"total_tokens":579}}"#;
        assert_eq!(extract_total_tokens(body), Some(579));
        assert_eq!(extract_total_tokens(br#"{"usage":{}}"#), None);
    }

    #[tokio::test]
    async fn id_only_quota_charges_daily_usage() {
        let store = QuotaStore::connect_memory().await.unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 6, 29).unwrap();
        let principal = Principal::IdOnly { id: "a".into() };

        let target = store.reserve_for_date(&principal, &quota(), date).await.unwrap();
        assert_eq!(target, ChargeTarget::IdOnly);
        store
            .charge_for_date(&principal, target, 8, date)
            .await
            .unwrap();
        assert_eq!(store.daily_usage("id", "a", date).await.unwrap(), 8);

        store
            .charge_for_date(&principal, target, 3, date)
            .await
            .unwrap();
        assert!(matches!(
            store.reserve_for_date(&principal, &quota(), date).await,
            Err(QuotaError::QuotaExceeded)
        ));
    }

    #[tokio::test]
    async fn verified_user_syncs_usage_to_higher_value() {
        let store = QuotaStore::connect_memory().await.unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 6, 29).unwrap();
        let principal = Principal::IdAndPhone {
            id: "a".into(),
            phone: "p".into(),
        };

        store
            .charge_for_date(
                &Principal::IdOnly { id: "a".into() },
                ChargeTarget::IdOnly,
                12,
                date,
            )
            .await
            .unwrap();

        let target = store.reserve_for_date(&principal, &quota(), date).await.unwrap();
        assert_eq!(target, ChargeTarget::VerifiedDaily);
        assert_eq!(store.daily_usage("id", "a", date).await.unwrap(), 12);
        assert_eq!(store.daily_usage("phone", "p", date).await.unwrap(), 12);
    }

    #[tokio::test]
    async fn verified_user_can_spend_phone_pool_after_daily_limit() {
        let store = QuotaStore::connect_memory().await.unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 6, 29).unwrap();
        let principal = Principal::IdAndPhone {
            id: "a".into(),
            phone: "p".into(),
        };

        store
            .charge_for_date(&principal, ChargeTarget::VerifiedDaily, 20, date)
            .await
            .unwrap();
        store.set_phone_pool("p", 15).await.unwrap();

        let target = store.reserve_for_date(&principal, &quota(), date).await.unwrap();
        assert_eq!(target, ChargeTarget::PhonePool);
        store
            .charge_for_date(&principal, target, 9, date)
            .await
            .unwrap();
        assert_eq!(store.pool_balance("p").await.unwrap(), 6);
    }
}
