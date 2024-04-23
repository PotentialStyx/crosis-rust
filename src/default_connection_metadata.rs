use async_trait::async_trait;

use crate::{
    ConnectionMetadataFetcher, FetchConnectionMetadataError, FetchConnectionMetadataResult,
};

#[cfg_attr(doc, doc(cfg(feature = "builtin_connection_metadata")))]
pub struct DefaultConnectionMetadataFetcher {
    pub connect_sid: String,
    pub replid: String,
}

#[async_trait]
impl ConnectionMetadataFetcher for DefaultConnectionMetadataFetcher {
    async fn fetch(&self) -> FetchConnectionMetadataResult {
        let client = reqwest::Client::new();
        let response = match client
            .post(format!(
                "https://replit.com/data/repls/{}/get_connection_metadata",
                self.replid
            ))
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Referer", "https://replit.com")
            .header("User-Agent", "Mozilla/5.0 (Rust Crosis)")
            .header("Cookie", format!("connect.sid={}", self.connect_sid))
            .body("{}")
            .send()
            .await
        {
            Ok(resp) => resp,
            // TODO: log error once tracing
            Err(err) => {
                eprintln!("{}", err);
                return Err(FetchConnectionMetadataError::Abort);
            }
        };

        if response.status() != 200 {
            if response.status().as_u16() > 500 {
                return Err(FetchConnectionMetadataError::Retriable);
            }

            // TODO: log error once tracing

            eprintln!("{:#?}", response.text().await);
            return Err(FetchConnectionMetadataError::Abort);
        }

        match response.json().await {
            Ok(resp) => Ok(resp),
            // TODO: log error once tracing
            Err(err) => {
                eprintln!("{}", err);
                return Err(FetchConnectionMetadataError::Abort);
            }
        }
    }
}
