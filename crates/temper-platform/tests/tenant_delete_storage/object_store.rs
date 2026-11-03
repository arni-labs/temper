//! An object store in memory that answers the S3 calls a tenant's removal
//! makes: ListObjectsV2 (1,000 keys a page, as S3 does) and DeleteObjects (at
//! most 1,000 keys and a Content-MD5, as S3 requires), plus object PUT, GET
//! and HEAD.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The most keys S3 lists in one page and deletes in one DeleteObjects call.
const S3_MAX_KEYS: usize = 1000;
pub const BUCKET: &str = "bucket";

#[derive(Clone, Default)]
pub struct ObjectStore {
    objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    /// DeleteObjects calls answered.
    pub delete_calls: Arc<AtomicUsize>,
    /// Answer DeleteObjects with 403 AccessDenied, as for credentials without
    /// the permission.
    pub refuse_deletes: Arc<AtomicBool>,
}

impl ObjectStore {
    pub async fn serve(&self) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(self.clone())
            .mount(&server)
            .await;
        server
    }

    pub fn put(&self, key: &str) {
        self.objects
            .lock()
            .expect("objects")
            .insert(key.to_string(), key.as_bytes().to_vec());
    }

    pub fn keys_under(&self, prefix: &str) -> usize {
        self.objects
            .lock()
            .expect("objects")
            .keys()
            .filter(|key| key.starts_with(prefix))
            .count()
    }

    pub fn holds(&self, key: &str) -> bool {
        self.objects.lock().expect("objects").contains_key(key)
    }

    fn list(&self, query: &BTreeMap<String, String>) -> ResponseTemplate {
        let prefix = query.get("prefix").cloned().unwrap_or_default();
        let after = query.get("continuation-token").cloned();
        let objects = self.objects.lock().expect("objects");
        let mut keys = objects
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .filter(|key| after.as_ref().is_none_or(|after| *key > after));
        let page: Vec<&String> = keys.by_ref().take(S3_MAX_KEYS).collect();
        let truncated = keys.next().is_some();
        let mut body = format!("<ListBucketResult><IsTruncated>{truncated}</IsTruncated>");
        for key in &page {
            body.push_str(&format!("<Contents><Key>{key}</Key></Contents>"));
        }
        if truncated && let Some(last) = page.last() {
            body.push_str(&format!(
                "<NextContinuationToken>{last}</NextContinuationToken>"
            ));
        }
        body.push_str("</ListBucketResult>");
        ResponseTemplate::new(200).set_body_string(body)
    }

    fn delete(&self, request: &Request) -> ResponseTemplate {
        if self.refuse_deletes.load(Ordering::SeqCst) {
            return ResponseTemplate::new(403)
                .set_body_string("<Error><Code>AccessDenied</Code></Error>");
        }
        if !request.headers.contains_key("content-md5") {
            return ResponseTemplate::new(400)
                .set_body_string("<Error><Code>MissingContentMD5</Code></Error>");
        }
        let body = String::from_utf8_lossy(&request.body);
        let keys: Vec<&str> = body
            .split("<Key>")
            .skip(1)
            .filter_map(|rest| rest.split_once("</Key>").map(|(key, _)| key))
            .collect();
        if keys.len() > S3_MAX_KEYS {
            return ResponseTemplate::new(400)
                .set_body_string("<Error><Code>MalformedXML</Code></Error>");
        }
        let mut objects = self.objects.lock().expect("objects");
        for key in keys {
            objects.remove(key);
        }
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_string("<DeleteResult></DeleteResult>")
    }
}

impl Respond for ObjectStore {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let query: BTreeMap<String, String> = request.url.query_pairs().into_owned().collect();
        let key = request
            .url
            .path()
            .trim_start_matches('/')
            .strip_prefix(BUCKET)
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string();
        match request.method.as_str() {
            "GET" if query.contains_key("list-type") => self.list(&query),
            "POST" if query.contains_key("delete") => self.delete(request),
            "PUT" => {
                self.objects
                    .lock()
                    .expect("objects")
                    .insert(key, request.body.clone());
                ResponseTemplate::new(200)
            }
            "GET" | "HEAD" => match self.objects.lock().expect("objects").get(&key) {
                Some(bytes) => ResponseTemplate::new(200).set_body_bytes(bytes.clone()),
                None => ResponseTemplate::new(404),
            },
            _ => ResponseTemplate::new(405),
        }
    }
}
