#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    English,
    Russian,
}

impl Language {
    pub fn parse(code: &str) -> Option<Self> {
        if code.eq_ignore_ascii_case("en") {
            Some(Self::English)
        } else if code.eq_ignore_ascii_case("ru") {
            Some(Self::Russian)
        } else {
            None
        }
    }

    pub const fn code(self) -> &'static str {
        match self {
            Self::English => "EN",
            Self::Russian => "RU",
        }
    }

    pub const fn text(self) -> &'static Text {
        match self {
            Self::English => &ENGLISH,
            Self::Russian => &RUSSIAN,
        }
    }
}

pub struct Text {
    pub usage: &'static str,
    pub startup_error: &'static str,
    pub language_error: &'static str,
    pub monitor_error: &'static str,
    pub corner_error: &'static str,
    pub autostart_add_error: &'static str,
    pub autostart_remove_error: &'static str,
    pub autostart_read_error: &'static str,
    pub autostart_path_too_long: &'static str,
    pub quit_error: &'static str,
    pub tray_language: &'static str,
    pub tray_monitor: &'static str,
    pub tray_monitor_main: &'static str,
    pub tray_monitor_unavailable: &'static str,
    pub tray_corner: &'static str,
    pub tray_top_right: &'static str,
    pub tray_top_left: &'static str,
    pub tray_bottom_right: &'static str,
    pub tray_bottom_left: &'static str,
    pub tray_autostart: &'static str,
    pub tray_close: &'static str,
    pub wallpaper_error: &'static str,
    pub poll_error: &'static str,
    pub title: &'static str,
    pub waiting: &'static str,
    pub first_query: &'static str,
    pub demo: &'static str,
    pub absent: &'static str,
    pub cable: &'static str,
    pub receiver_confirmed: &'static str,
    pub receiver_no_response: &'static str,
    pub receiver: &'static str,
    pub ambiguous: &'static str,
    pub unavailable: &'static str,
    pub query_failed: &'static str,
    pub battery_unknown: &'static str,
    pub charging: &'static str,
    pub fully_charged: &'static str,
    pub last_query: &'static str,
    pub next_query: &'static str,
    pub seconds: &'static str,
    pub minutes: &'static str,
    pub hours: &'static str,
}

const ENGLISH: Text = Text {
    usage: "Usage\n  lamzu-widget [--demo] [--lang EN|RU]\n  lamzu-widget [--quit | --autostart | -a | --autostart-remove | -arm | --help | -h]",
    startup_error: "Could not start the widget.",
    language_error: "Could not save the widget language.",
    monitor_error: "Could not save the widget monitor.",
    corner_error: "Could not save the widget corner.",
    autostart_add_error: "Could not enable startup at sign-in.",
    autostart_remove_error: "Could not disable startup at sign-in.",
    autostart_read_error: "Could not read the startup setting.",
    autostart_path_too_long: "The executable path is too long for startup at sign-in.",
    quit_error: "Could not close the widget.",
    tray_language: "Language",
    tray_monitor: "Monitor",
    tray_monitor_main: "Primary monitor",
    tray_monitor_unavailable: "Selected monitor is disconnected",
    tray_corner: "Position",
    tray_top_right: "Top-right corner",
    tray_top_left: "Top-left corner",
    tray_bottom_right: "Bottom-right corner",
    tray_bottom_left: "Bottom-left corner",
    tray_autostart: "Start at sign-in",
    tray_close: "Close application",
    wallpaper_error: "Wallpaper layer",
    poll_error: "Mouse query",
    title: "LAMZU Mouse",
    waiting: "Looking for the mouse…",
    first_query: "Waiting for the first reading",
    demo: "Connected via USB · demo",
    absent: "Mouse not found",
    cable: "Connected via USB",
    receiver_confirmed: "2.4 GHz link confirmed",
    receiver_no_response: "Receiver · mouse did not respond",
    receiver: "Receiver connected",
    ambiguous: "Multiple devices found",
    unavailable: "Connection status unavailable",
    query_failed: "Battery reading unavailable",
    battery_unknown: "Battery level unknown",
    charging: "Charging",
    fully_charged: "Fully charged",
    last_query: "Last query",
    next_query: "Until next query",
    seconds: "s",
    minutes: "min",
    hours: "h",
};

const RUSSIAN: Text = Text {
    usage: "Использование\n  lamzu-widget [--demo] [--lang EN|RU]\n  lamzu-widget [--quit | --autostart | -a | --autostart-remove | -arm | --help | -h]",
    startup_error: "Не удалось запустить виджет.",
    language_error: "Не удалось сохранить язык виджета.",
    monitor_error: "Не удалось сохранить выбранный монитор.",
    corner_error: "Не удалось сохранить выбранный угол.",
    autostart_add_error: "Не удалось включить автозапуск.",
    autostart_remove_error: "Не удалось выключить автозапуск.",
    autostart_read_error: "Не удалось прочитать настройку автозапуска.",
    autostart_path_too_long: "Путь к программе слишком длинный для автозапуска.",
    quit_error: "Не удалось закрыть виджет.",
    tray_language: "Язык",
    tray_monitor: "Монитор",
    tray_monitor_main: "Главный монитор",
    tray_monitor_unavailable: "Выбранный монитор отключён",
    tray_corner: "Расположение",
    tray_top_right: "Правый верхний угол",
    tray_top_left: "Левый верхний угол",
    tray_bottom_right: "Правый нижний угол",
    tray_bottom_left: "Левый нижний угол",
    tray_autostart: "Автозапуск",
    tray_close: "Закрыть приложение",
    wallpaper_error: "Слой обоев",
    poll_error: "Проверка мыши",
    title: "Мышь LAMZU",
    waiting: "Поиск мыши…",
    first_query: "Ожидаем первый замер",
    demo: "Подключена по USB · демонстрация",
    absent: "Мышь не найдена",
    cable: "Подключена по USB",
    receiver_confirmed: "Связь 2,4 ГГц подтверждена",
    receiver_no_response: "Приёмник · мышь не ответила",
    receiver: "Приёмник подключён",
    ambiguous: "Найдено несколько устройств",
    unavailable: "Статус недоступен",
    query_failed: "Замер недоступен",
    battery_unknown: "Заряд пока неизвестен",
    charging: "Зарядка",
    fully_charged: "Полный заряд",
    last_query: "Последний замер",
    next_query: "До следующего запроса",
    seconds: "сек",
    minutes: "мин",
    hours: "ч",
};

#[cfg(test)]
mod tests {
    use super::Language;

    #[test]
    fn accepts_only_supported_language_codes() {
        assert_eq!(Language::default(), Language::English);
        assert_eq!(Language::parse("EN"), Some(Language::English));
        assert_eq!(Language::parse("rU"), Some(Language::Russian));
        assert_eq!(Language::parse("de"), None);
        assert_eq!(Language::Russian.code(), "RU");
    }
}
