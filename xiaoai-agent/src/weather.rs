use std::sync::Arc;

use anyhow::Context;
use reqwest::Client;
use serde_json::{json, Value};
use url::Url;

use crate::config::{timeout_duration, AppConfig, WeatherConfig};

#[derive(Clone)]
pub struct WeatherService {
    config: Arc<AppConfig>,
    client: Client,
}

impl WeatherService {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    pub async fn get_weather(&self, location: &str) -> Value {
        match self.get_weather_inner(location.trim()).await {
            Ok(value) => value,
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    async fn get_weather_inner(&self, requested_location: &str) -> anyhow::Result<Value> {
        let settings = &self.config.agent.weather;
        anyhow::ensure!(
            !settings.qweather_url.trim().is_empty(),
            "agent.weather.qweather_url is empty"
        );

        let (location, label) = if requested_location.is_empty() {
            self.auto_ip_location(settings).await.unwrap_or_else(|err| {
                (
                    settings.default_location.clone(),
                    format!("default location {} ({err})", settings.default_location),
                )
            })
        } else if looks_like_qweather_location(requested_location) {
            (
                requested_location.to_string(),
                requested_location.to_string(),
            )
        } else {
            self.lookup_city(settings, requested_location)
                .await
                .unwrap_or_else(|err| {
                    (
                        requested_location.to_string(),
                        format!("{requested_location} ({err})"),
                    )
                })
        };

        let mut url = Url::parse(&settings.qweather_url)?;
        replace_query_pairs(&mut url, &[("location", &location)]);
        let data = self.fetch_json(url, settings).await?;
        Ok(summarize_daily_weather(&data, &label))
    }

    async fn auto_ip_location(&self, settings: &WeatherConfig) -> anyhow::Result<(String, String)> {
        let data = self
            .client
            .get(&settings.ip_lookup_url)
            .timeout(timeout_duration(settings.timeout_s))
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        let latitude = data
            .get("latitude")
            .or_else(|| data.get("lat"))
            .and_then(Value::as_f64);
        let longitude = data
            .get("longitude")
            .or_else(|| data.get("lon"))
            .and_then(Value::as_f64);
        let (Some(lat), Some(lon)) = (latitude, longitude) else {
            anyhow::bail!("IP location response does not contain latitude/longitude");
        };
        let label = [
            data.get("city").and_then(Value::as_str),
            data.get("region")
                .or_else(|| data.get("regionName"))
                .and_then(Value::as_str),
            data.get("country_name")
                .or_else(|| data.get("country"))
                .and_then(Value::as_str),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
        Ok((format!("{lon:.4},{lat:.4}"), label))
    }

    async fn lookup_city(
        &self,
        settings: &WeatherConfig,
        location: &str,
    ) -> anyhow::Result<(String, String)> {
        let mut url = Url::parse(&settings.qweather_url)?;
        url.set_path("/geo/v2/city/lookup");
        replace_query_pairs(&mut url, &[("location", location), ("number", "1")]);
        let data = self.fetch_json(url, settings).await?;
        let item = data
            .get("location")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .context("QWeather city lookup returned no candidates")?;
        let resolved = item
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| {
                let lon = item.get("lon")?.as_str()?;
                let lat = item.get("lat")?.as_str()?;
                Some(format!("{lon},{lat}"))
            })
            .context("QWeather city lookup candidate has no id or coordinates")?;
        let label = ["name", "adm2", "adm1", "country"]
            .iter()
            .filter_map(|key| item.get(key).and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        Ok((resolved, label))
    }

    async fn fetch_json(&self, url: Url, settings: &WeatherConfig) -> anyhow::Result<Value> {
        let response = self
            .client
            .get(url)
            .timeout(timeout_duration(settings.timeout_s))
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        anyhow::ensure!(
            status.is_success(),
            "QWeather HTTP {status}: {}",
            preview(&text)
        );
        serde_json::from_str(&text)
            .with_context(|| format!("QWeather returned non-JSON body: {}", preview(&text)))
    }
}

fn looks_like_qweather_location(location: &str) -> bool {
    let location = location.trim();
    if location.is_empty() {
        // 空串会让下面的 all() 恒为 true，被当成「坐标」直接发给 QWeather。
        // 调用方目前先 trim + 判空，所以这里是加固而不是修线上 bug。
        return false;
    }
    location.contains(',') || location.chars().all(|ch| ch.is_ascii_digit())
}

fn replace_query_pairs(url: &mut Url, replacements: &[(&str, &str)]) {
    let mut pairs = url
        .query_pairs()
        .filter(|(key, _)| !replacements.iter().any(|(name, _)| key == *name))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    pairs.extend(
        replacements
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string())),
    );
    url.set_query(None);
    url.query_pairs_mut().extend_pairs(pairs);
}

fn preview(text: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut value = text.chars().take(MAX_CHARS).collect::<String>();
    if text.chars().count() > MAX_CHARS {
        value.push_str("...");
    }
    value
}

fn summarize_daily_weather(data: &Value, label: &str) -> Value {
    let Some(daily) = data.get("daily").and_then(Value::as_array) else {
        return json!({"location": label, "raw": data});
    };
    let forecast = daily
        .iter()
        .take(3)
        .map(|item| {
            json!({
                "date": item.get("fxDate"),
                "day": item.get("textDay"),
                "night": item.get("textNight"),
                "temp_min_c": item.get("tempMin"),
                "temp_max_c": item.get("tempMax"),
                "humidity": item.get("humidity"),
                "wind": item.get("windDirDay"),
                "wind_scale": item.get("windScaleDay"),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "location": label,
        "qweather_code": data.get("code"),
        "updated_at": data.get("updateTime"),
        "forecast": forecast,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qweather_location_detection() {
        // 有逗号的经纬度、纯数字的 location id 才算「已经是 QWeather 能认的坐标」。
        // 注意：纯数字一律视为 location id，本函数不校验它是否真实存在
        //（例如带国家码的 "86101180101" 也会被直接发出去）。
        assert!(looks_like_qweather_location("114.1,30.52"));
        assert!(looks_like_qweather_location("101180101"));
        assert!(looks_like_qweather_location(" 101180101 "));
        // 城市名要走 lookup，不能直接发
        assert!(!looks_like_qweather_location("郑州"));
        assert!(!looks_like_qweather_location("zhengzhou"));
        // 加固：空串会让 all() 恒真，函数自身必须拒绝。调用方 get_weather 目前
        // 已先 trim 并走 is_empty 分支，所以线上不可达——这里防的是以后有人
        // 绕过那层判断直接调用它。
        assert!(!looks_like_qweather_location(""));
        assert!(!looks_like_qweather_location("   "));
    }

    #[test]
    fn replace_query_pairs_overrides_without_duplicating() {
        let mut url = Url::parse(
            "https://devapi.qweather.com/v7/weather/3d?location=beijing&key=old&lang=zh",
        )
        .unwrap();
        replace_query_pairs(&mut url, &[("location", "101180101"), ("key", "new")]);
        let pairs = url
            .query_pairs()
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(pairs.get("location").map(String::as_str), Some("101180101"));
        assert_eq!(pairs.get("key").map(String::as_str), Some("new"));
        // 没被替换的参数要原样保留
        assert_eq!(pairs.get("lang").map(String::as_str), Some("zh"));
        assert_eq!(
            url.query_pairs().filter(|(key, _)| key == "key").count(),
            1,
            "同一个参数不能出现两次"
        );
    }

    #[test]
    fn preview_truncates_error_bodies() {
        assert_eq!(preview("abc"), "abc");
        let long = "字".repeat(300);
        let previewed = preview(&long);
        assert!(previewed.ends_with("..."));
        // 240 个字符 + 三个点
        assert_eq!(previewed.chars().count(), 243);
    }

    #[test]
    fn summarize_daily_weather_keeps_at_most_three_days() {
        let data = json!({
            "code": "200",
            "updateTime": "2026-10-24T13:00+08:00",
            "daily": [
                {"fxDate": "2026-10-24", "textDay": "晴", "textNight": "多云", "tempMin": "10", "tempMax": "22", "humidity": "40", "windDirDay": "东北风", "windScaleDay": "3-4"},
                {"fxDate": "2026-10-25", "textDay": "阴", "textNight": "小雨", "tempMin": "11", "tempMax": "20", "humidity": "55", "windDirDay": "东风", "windScaleDay": "2"},
                {"fxDate": "2026-10-26", "textDay": "小雨", "textNight": "阴", "tempMin": "9", "tempMax": "17", "humidity": "70", "windDirDay": "北风", "windScaleDay": "4-5"},
                {"fxDate": "2026-10-27", "textDay": "晴", "textNight": "晴", "tempMin": "8", "tempMax": "19", "humidity": "45", "windDirDay": "西风", "windScaleDay": "3"}
            ]
        });
        let summary = summarize_daily_weather(&data, "郑州");
        assert_eq!(summary["location"], "郑州");
        assert_eq!(summary["qweather_code"], "200");
        let forecast = summary["forecast"].as_array().unwrap();
        assert_eq!(forecast.len(), 3, "最多取三天，避免把整段 JSON 念出来");
        assert_eq!(forecast[0]["date"], "2026-10-24");
        assert_eq!(forecast[0]["day"], "晴");
        assert_eq!(forecast[0]["temp_min_c"], "10");
        assert_eq!(forecast[2]["wind_scale"], "4-5");
    }

    #[test]
    fn summarize_daily_weather_falls_back_to_raw_body() {
        let data = json!({"error": "no daily field"});
        let summary = summarize_daily_weather(&data, "郑州");
        assert_eq!(summary["location"], "郑州");
        assert_eq!(summary["raw"]["error"], "no daily field");
        assert!(summary.get("forecast").is_none());
    }
}
