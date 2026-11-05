use super::*;

#[test]
fn fresh_config_discovers_driver_from_relocated_app_without_saving_paths() {
    let config = Config::default();
    for bundle in [
        "/Applications/Timelord.app",
        "/Users/test/Downloads/My Timelord.app",
    ] {
        let helper = Path::new(bundle).join("Contents/Helpers/timelord-service");
        assert_eq!(
            driver_path(&config, &helper),
            Path::new(bundle).join("Contents/Helpers/../Frameworks/libftd2xx.dylib")
        );
    }
    assert!(config.hardware.client_binary.is_none());
    assert!(config.hardware.driver_library.is_none());
}

#[test]
fn explicit_driver_and_external_client_locations_remain_authoritative() {
    let mut config = Config::default();
    let helper = Path::new("/Applications/Timelord.app/Contents/Helpers/timelord-service");
    config.hardware.client_binary = Some("/custom/Helpers/hw_vdf_client".into());
    assert_eq!(
        driver_path(&config, helper),
        Path::new("/custom/Helpers/../Frameworks/libftd2xx.dylib")
    );
    config.hardware.driver_library = Some("/custom/driver.dylib".into());
    assert_eq!(
        driver_path(&config, helper),
        Path::new("/custom/driver.dylib")
    );
}
