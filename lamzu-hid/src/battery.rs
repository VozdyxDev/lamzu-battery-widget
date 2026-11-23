use crate::ChargeState;

// The Maya BatteryParam voltage knots in the bundled Config.ini (one per 5%).
// The first field in the file is an option, followed by these 21 knots.
const VOLTAGE_MV: [u16; 21] = [
    3050, 3420, 3480, 3540, 3600, 3660, 3720, 3760, 3800, 3840, 3880, 3920, 3940, 3960, 3980, 4000,
    4020, 4040, 4060, 4080, 4110,
];

pub(crate) fn manufacturer_percent(raw_percent: u8, millivolts: u16, state: ChargeState) -> u8 {
    if millivolts == 0 {
        return raw_percent;
    }
    let upper = VOLTAGE_MV.partition_point(|&threshold| threshold <= millivolts);
    if upper == 0 {
        return 1;
    }
    if upper == VOLTAGE_MV.len() {
        return if state == ChargeState::Charging {
            99
        } else {
            100
        };
    }

    let lower = upper - 1;
    let start = VOLTAGE_MV[lower];
    let step = (VOLTAGE_MV[upper] - start) / 5;
    let percent = (lower as u8) * 5 + ((millivolts - start) / step) as u8;
    match percent {
        0 | 15 => percent + 1,
        _ => percent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_maya_calibration_instead_of_the_raw_high_charge_reading() {
        use ChargeState::{Charging, Discharging, Full};

        assert_eq!(manufacturer_percent(95, 4080, Discharging), 95);
        assert_eq!(manufacturer_percent(95, 4103, Discharging), 98);
        assert_eq!(manufacturer_percent(95, 4104, Discharging), 99);
        assert_eq!(manufacturer_percent(95, 4109, Discharging), 99);
        assert_eq!(manufacturer_percent(95, 4110, Charging), 99);
        assert_eq!(manufacturer_percent(95, 4113, Charging), 99);
        assert_eq!(manufacturer_percent(95, 4113, Discharging), 100);
        assert_eq!(manufacturer_percent(95, 4113, Full), 100);
    }

    #[test]
    fn preserves_native_low_charge_rules_and_the_no_voltage_fallback() {
        let state = ChargeState::Discharging;
        assert_eq!(manufacturer_percent(75, 0, state), 75);
        assert_eq!(manufacturer_percent(0, 3049, state), 1);
        assert_eq!(manufacturer_percent(0, 3050, state), 1);
        assert_eq!(manufacturer_percent(15, 3540, state), 16);
        assert_eq!(manufacturer_percent(15, 3551, state), 16);
        assert_eq!(manufacturer_percent(15, 3552, state), 16);
        assert_eq!(manufacturer_percent(85, 4055, state), 88);
    }
}
