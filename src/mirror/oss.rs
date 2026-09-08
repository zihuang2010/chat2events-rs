//! 私有 OSS 的 GET 请求：直连 bucket，以请求头携带 V4 签名。

use super::{
    download::{CONNECT_TIMEOUT, TIMEOUT},
    error::{MirrorError, Result},
};
use crate::config::{OssConfig, OssSecrets};
use reqsign_aliyun_oss::{Credential, RequestSigner, SigningVersion};
use reqsign_core::{Context, SignRequest};

/// 每轮下载共用连接池和 RAM 凭证；签名在每次请求时生成，不缓存带日期的鉴权头。
pub(super) struct OssClient {
    pub(super) http: reqwest::Client,
    base: reqwest::Url,
    signer: RequestSigner,
    credential: Credential,
}

impl OssClient {
    #[cfg(test)]
    pub(super) fn for_test(base: &str) -> Self {
        let mut client = Self::new(
            &OssConfig {
                endpoint: "https://oss-cn-zhangjiakou.aliyuncs.com".into(),
                bucket: "jdd-rh-wechat".into(),
                region: "cn-zhangjiakou".into(),
            },
            &OssSecrets {
                access_key_id: "test-access-key".into(),
                access_key_secret: "test-secret-key".into(),
            },
        )
        .unwrap();
        client.base = base.parse().unwrap();
        client
    }

    /// 配置错误影响整轮，构造时即报错；错误文案不回显端点或凭证原文。
    pub(super) fn new(cfg: &OssConfig, secrets: &OssSecrets) -> Result<Self> {
        let invalid = || {
            MirrorError::Round("OSS 配置无效：需要 HTTPS 服务端点（不含 bucket、路径或凭证）、bucket、region 和非空 AccessKey".into())
        };
        let mut base = reqwest::Url::parse(&cfg.endpoint).map_err(|_| invalid())?;
        if base.scheme() != "https"
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.path() != "/"
            || base.query().is_some()
            || base.fragment().is_some()
            || cfg.bucket.is_empty()
            || !cfg
                .bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || cfg.region.trim().is_empty()
            || secrets.access_key_id.trim().is_empty()
            || secrets.access_key_secret.trim().is_empty()
        {
            return Err(invalid());
        }
        // endpoint 是地域服务域名；实际对象请求使用 bucket 子域名。
        let host = format!("{}.{}", cfg.bucket, base.host_str().unwrap());
        base.set_host(Some(&host)).map_err(|_| invalid())?;
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            // 签名绑定当前目标；重定向须作为错误暴露，避免变成匿名请求。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| MirrorError::Round(format!("OSS 客户端构建失败：{e}")))?;
        Ok(Self {
            http,
            base,
            signer: RequestSigner::new(&cfg.bucket)
                .with_region(&cfg.region)
                .with_signing_version(SigningVersion::V4),
            credential: Credential {
                access_key_id: secrets.access_key_id.clone(),
                access_key_secret: secrets.access_key_secret.clone(),
                security_token: None,
                expires_in: None,
            },
        })
    }

    /// 每次尝试重新签名；object_key 是原始对象名，不能当 URL 拼接。
    pub(super) async fn request(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<reqwest::Request> {
        if object_key.is_empty() || object_key.split('/').any(|s| s == "." || s == "..") {
            return Err(MirrorError::Room(
                "OSS object_key 为空或包含 URL 会归一化的路径段".into(),
            ));
        }
        let mut url = self.base.clone();
        // 保留对象名中的目录分隔符，对中文、空格、?、# 等逐段编码后再签名。
        // 直接拼接会把对象名的一部分误当成查询参数或片段，导致读取错误对象或签名失败。
        url.path_segments_mut()
            .expect("构造时已确认 HTTPS 端点")
            .clear()
            .extend(object_key.split('/'));
        let mut parts = http::Request::get(url.as_str())
            .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
            // 起点越界时要求 OSS 返回 416，不忽略 Range 返回整个文件。
            .header("x-oss-range-behavior", "standard")
            .body(())
            .map_err(|e| MirrorError::Room(format!("OSS 请求构建失败：{e}")))?
            .into_parts()
            .0;
        // None 表示请求头签名，不生成带签名参数的 URL；重试也按当前时间重新签名。
        self.signer
            .sign_request(&Context::new(), &mut parts, Some(&self.credential), None)
            .await
            .map_err(|_| {
                MirrorError::Room("OSS V4 签名失败，请检查地域和 AccessKey 配置".into())
            })?;
        // HeaderValue 的 Debug 输出会隐藏此值，避免调试请求时暴露鉴权头。
        parts
            .headers
            .get_mut(reqwest::header::AUTHORIZATION)
            .expect("V4 请求头签名已成功")
            .set_sensitive(true);
        Ok(self.http.get(url).headers(parts.headers).build()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 人工联调：只查一行索引、读取两个小范围，不落盘、不调用模型、不写库。
    #[tokio::test]
    #[ignore = "需要本机 secrets.toml、真实 MySQL 和 OSS 读取权限"]
    async fn live_private_bucket_range_read() -> crate::Result<()> {
        use sqlx::Row;

        let (cfg, secrets) =
            crate::config::load_from_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
        let client = OssClient::new(&cfg.ingest.oss, &secrets.oss)?;
        let pool = crate::config::mysql_pool(&cfg.mysql, &secrets.mysql.url).await?;
        let row = sqlx::query(
            "SELECT file_month, ndjson_object_key, ndjson_position \
             FROM b_wecom_group_message_month_file \
             WHERE file_status = 0 AND is_deleted = 0 AND ndjson_position > 1 \
             ORDER BY file_month DESC, ndjson_last_append_time DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await?;
        let month: String = row.try_get("file_month")?;
        let key: String = row.try_get("ndjson_object_key")?;
        let position: u64 = row.try_get("ndjson_position")?;
        println!("索引读取成功：月份 {month}，已确认 {position} 字节");
        for start in [0, position / 2] {
            let end = (start + 4095).min(position - 1);
            let request = client.request(&key, start, end).await?;
            let mut response = client.http.execute(request).await?;
            let status = response.status();
            let content_range = response
                .headers()
                .get("content-range")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("")
                .to_owned();
            println!("OSS 响应：HTTP {status}，Content-Range: {content_range}");
            assert_eq!(status, reqwest::StatusCode::PARTIAL_CONTENT);
            assert!(content_range.starts_with(&format!("bytes {start}-{end}/")));
            let expected = (end - start + 1) as usize;
            let mut received = 0;
            while let Some(chunk) = response.chunk().await? {
                received += chunk.len();
                assert!(received <= expected, "响应超过请求范围");
                if start == 0 && received == chunk.len() {
                    assert_eq!(chunk.first(), Some(&b'{'), "月文件未以 JSON 对象开始");
                }
            }
            assert_eq!(received, expected);
            println!("读取成功：bytes={start}-{end}，实际 {received} 字节，未输出正文");
        }
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn request_signs_the_encoded_object_path_and_bounded_range() {
        let cfg = OssConfig {
            endpoint: "https://oss-cn-zhangjiakou.aliyuncs.com".into(),
            bucket: "jdd-rh-wechat".into(),
            region: "cn-zhangjiakou".into(),
        };
        let secrets = OssSecrets {
            access_key_id: "test-access-key".into(),
            access_key_secret: "test-secret-key".into(),
        };
        let client = OssClient::new(&cfg, &secrets).unwrap();
        let request = client
            .request("202609/群聊 +?#%.ndjson", 8, 15)
            .await
            .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://jdd-rh-wechat.oss-cn-zhangjiakou.aliyuncs.com/202609/%E7%BE%A4%E8%81%8A%20+%3F%23%25.ndjson"
        );
        assert_eq!(request.headers()["range"], "bytes=8-15");
        assert_eq!(request.headers()["x-oss-range-behavior"], "standard");
        assert_eq!(
            request.headers()["x-oss-content-sha256"],
            "UNSIGNED-PAYLOAD"
        );
        let authorization = &request.headers()["authorization"];
        let auth = authorization.to_str().unwrap();
        assert!(auth.starts_with("OSS4-HMAC-SHA256 Credential=test-access-key/"));
        assert!(auth.contains("/cn-zhangjiakou/oss/aliyun_v4_request"));
        assert!(request.headers().contains_key("x-oss-date"));
        assert!(authorization.is_sensitive());
        assert!(!format!("{request:?}").contains("test-secret-key"));
        assert!(!request.url().as_str().contains("test-access-key"));
        assert!(client.request("a/../b", 0, 7).await.is_err());
        assert!(client.request("", 0, 7).await.is_err());
    }

    #[test]
    fn invalid_configuration_does_not_expose_credentials() {
        let cfg = OssConfig {
            endpoint: "https://user:secret@oss-cn-zhangjiakou.aliyuncs.com".into(),
            bucket: "jdd-rh-wechat".into(),
            region: "cn-zhangjiakou".into(),
        };
        let secrets = OssSecrets {
            access_key_id: "test-access-key".into(),
            access_key_secret: "test-secret-key".into(),
        };
        let error = OssClient::new(&cfg, &secrets).err().unwrap().to_string();
        assert!(error.contains("OSS 配置无效"));
        assert!(!error.contains("secret"));
        assert!(!error.contains("test-access-key"));
    }
}
