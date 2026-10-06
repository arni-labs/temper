use temper_wasm_sdk::prelude::*;
temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        let base = "http://127.0.0.1:38801/tdata";
        let query = ctx.http_get(&format!("{base}/Children?$filter=instance_id%20eq%20'i'"))?;
        let read = ctx.http_get(&format!("{base}/Children('other')"))?;
        let remove = ctx.http_post(&format!("{base}/Children('mine')/Probe.Remove"), "{}")?;
        let privileged = ctx.http_post(&format!("{base}/Children('mine')/Probe.ModuleOnly"), "{}")?;
        let shared = ctx.get_secret("webhook_token")?;
        let denied = ctx.get_secret("unrelated_token").is_err();
        let report = json!({"shared_secret_ok":(shared == "fixture-token").to_string(),"unrelated_secret_denied":denied.to_string(),"admitted":ctx.entity_state,"query_status":query.status.to_string(),"query_body":query.body,
                  "denied_read_status":read.status.to_string(),"remove_status":remove.status.to_string(),
                  "module_status":privileged.status.to_string(),"module_body":privileged.body});
        if let Some(raw) = &ctx.http_request {
            let inbound: temper_wasm_sdk::http_stream::InboundHttp = serde_json::from_value(raw.clone()).map_err(|e|e.to_string())?;
            inbound.submit_response_head(200, &[("content-type","application/json")]).map_err(|e|format!("{e:?}"))?;
            let mut body = inbound.response_body();
            body.write_all_chunk(report.to_string().as_bytes()).map_err(|e|format!("{e:?}"))?;
            body.finish().map_err(|e|format!("{e:?}"))?;
        }
        Ok(report)
    }
}
