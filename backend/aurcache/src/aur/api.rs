use anyhow::anyhow;
use aur_rs::{Package, Request};
use backon::{ConstantBuilder, FibonacciBuilder, Retryable};
use std::time::Duration;

// https://wiki.archlinux.org/title/Aurweb_RPC_interface
// API rate limit 4000 requests per day

pub async fn query_aur(query: &str) -> anyhow::Result<Vec<Package>> {
    let request = Request::default();
    let response = (|| async { request.search_package_by_name(query).await })
        .retry(
            ConstantBuilder::default()
                .with_max_times(3)
                .with_delay(Duration::from_millis(500)),
        )
        .await
        .map_err(|e| anyhow!("failed to get package: {}", e))?;

    let mut response = response.results;
    response.sort_by(|x, x1| x.popularity.partial_cmp(&x1.popularity).unwrap().reverse());

    Ok(response)
}

pub async fn get_info_by_name(pkg_name: &str) -> anyhow::Result<Package> {
    try_get_info_by_name(pkg_name)
        .await
        .and_then(|res| res.ok_or_else(|| anyhow!("no package found")))
}

pub async fn try_get_info_by_name(pkg_name: &str) -> anyhow::Result<Option<Package>> {
    let request = Request::default();
    let mut response = (|| async { request.search_info_by_name(pkg_name).await })
        .retry(
            FibonacciBuilder::default()
                .with_min_delay(Duration::from_millis(500))
                .with_max_times(4),
        )
        .await
        .map_err(|e| anyhow!("failed to get package: {}", e))?;

    let response = response.results.pop();

    Ok(response)
}
