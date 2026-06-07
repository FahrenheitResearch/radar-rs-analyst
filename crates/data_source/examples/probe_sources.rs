fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sites = data_source::list_recent_level2_sites(7)?;
    println!("level2_sites={}", sites.len());

    let site = sites
        .iter()
        .find(|site| site.level2_id == "KTLX")
        .cloned()
        .unwrap_or_else(|| data_source::RadarSite::new("KTLX"));

    let l2 = data_source::latest_level2_object(&site.level2_id, 7)?;
    println!("latest_l2={} bytes={}", l2.key, l2.size);

    Ok(())
}
