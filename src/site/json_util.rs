//! Small helpers shared by the JSON API site adapters (Rousi Pro, TNode, SunnyPT).

use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone};
use reqwest::header::HeaderMap;
use reqwest::{Client, Method, StatusCode};
use serde_json::Value;

/// Follow a dotted path such as `data.seeding_leeching_data.seeding_count`.
pub(crate) fn at<'a>(value: &'a Value, path: &str) -> &'a Value {
    path.split('.').fold(value, |value, key| &value[key])
}

pub(crate) fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().replace(',', "").parse().ok())
        .filter(|n| n.is_finite())
}

pub(crate) fn integer(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64))
        .or_else(|| value.as_str()?.trim().replace(',', "").parse().ok())
}

/// Byte counts given either as numbers or as display strings such as `1.5 TB` / `800 MiB`
/// (binary multiples, as NexusPHP renders them).
pub(crate) fn size_bytes(value: &Value) -> Option<u64> {
    if let Some(bytes) = value.as_u64() {
        return Some(bytes);
    }
    let raw = value.as_str()?.trim().replace(',', "");
    if let Ok(bytes) = raw.parse::<u64>() {
        return Some(bytes);
    }
    let split = raw
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(raw.len());
    let amount: f64 = raw[..split].trim().parse().ok()?;
    let unit = raw[split..].trim().to_ascii_uppercase();
    let power = match unit.trim_end_matches("IB").trim_end_matches('B') {
        "" => 0,
        "K" => 1,
        "M" => 2,
        "G" => 3,
        "T" => 4,
        "P" => 5,
        "E" => 6,
        _ => return None,
    };
    Some((amount * 1024_f64.powi(power)).round() as u64)
}

pub(crate) fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// RFC 3339, `YYYY-MM-DD HH:MM:SS` (UTC+8, as the Chinese trackers use) or a Unix
/// timestamp in seconds / milliseconds. Returns milliseconds.
pub(crate) fn time_millis(value: &Value) -> Option<i64> {
    if let Some(number) = value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())
    {
        return (number > 0).then(|| {
            if number < 100_000_000_000 {
                number * 1000
            } else {
                number
            }
        });
    }
    let raw = value.as_str()?.trim();
    if let Ok(time) = DateTime::parse_from_rfc3339(raw) {
        return Some(time.timestamp_millis());
    }
    let time = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    FixedOffset::east_opt(8 * 3600)?
        .from_local_datetime(&time)
        .single()
        .map(|time| time.timestamp_millis())
}

/// Send a request and decode a JSON body. `label` names the site in error messages.
pub(crate) async fn request_json(
    client: &Client,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: Option<&Value>,
    label: &str,
) -> Result<Value, String> {
    let mut request = client.request(method, url).headers(headers);
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("{label} 请求失败: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("{label} 读取响应失败: {error}"))?;
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        if body.contains("cf-chl") || body.contains("challenge-platform") {
            return Err(format!("{label} 被 Cloudflare 验证页拦截"));
        }
        return Err(format!("{label} 认证失败（HTTP {status}），请检查凭据"));
    }
    if !status.is_success() {
        return Err(format!("{label} 返回 HTTP {status}"));
    }
    serde_json::from_str(&body).map_err(|_| format!("{label} 返回的不是有效 JSON"))
}

/// `{code: 0, ...}` / `{status: 200, ...}` envelopes. Missing codes are accepted.
pub(crate) fn ensure_success(json: &Value, label: &str) -> Result<(), String> {
    let ok = |value: &Value, expected: &[i64]| {
        value
            .as_i64()
            .or_else(|| value.as_str()?.parse().ok())
            .is_some_and(|code| expected.contains(&code))
    };
    let failed = (json.get("code").is_some() && !ok(&json["code"], &[0, 200]))
        || (json.get("status").is_some_and(|status| !status.is_string())
            && !ok(&json["status"], &[0, 200]));
    if failed {
        let message = ["message", "msg", "error"]
            .iter()
            .find_map(|key| json[*key].as_str())
            .unwrap_or("请求失败");
        return Err(format!("{label} 接口错误: {message}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_numbers_and_times_leniently() {
        let value = json!({"a": {"b": "1,024"}, "t": 1_700_000_000, "s": "2026-01-02 03:04:05"});
        assert_eq!(integer(at(&value, "a.b")), Some(1024));
        assert_eq!(time_millis(&value["t"]), Some(1_700_000_000_000));
        assert_eq!(time_millis(&value["s"]), Some(1_767_294_245_000));
        assert_eq!(
            time_millis(&json!("2026-01-02T03:04:05Z")),
            Some(1_767_323_045_000)
        );
    }

    #[test]
    fn parses_display_sizes() {
        assert_eq!(size_bytes(&json!(42)), Some(42));
        assert_eq!(size_bytes(&json!("1.5 TB")), Some(1_649_267_441_664));
        assert_eq!(size_bytes(&json!("800 MiB")), Some(838_860_800));
        assert_eq!(size_bytes(&json!("0 B")), Some(0));
        assert_eq!(size_bytes(&json!("---")), None);
    }

    #[test]
    fn envelope_codes() {
        assert!(ensure_success(&json!({"code": 0, "data": {}}), "x").is_ok());
        assert!(ensure_success(&json!({"status": 200, "data": {}}), "x").is_ok());
        assert!(ensure_success(&json!({"data": {}}), "x").is_ok());
        assert!(ensure_success(&json!({"code": 401, "message": "bad key"}), "x").is_err());
    }
}
