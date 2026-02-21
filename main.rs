use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box, Button, ComboBoxText, Label, Orientation, ProgressBar,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

mod burn_engine;
use burn_engine::{BurnConfig, BurnEngine, BurnEvent};

fn main() -> gtk4::glib::ExitCode {
    let app = Application::builder()
        .application_id("com.shay.icedburn.pro")
        .build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &Application) {
    // 1. עיצוב קרבי (CSS) - הפס הכתום והרקע הכהה
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(r#"
        window { background-color: #1e1e2e; color: #cdd6f4; }
        .refresh-button { background-color: #313244; color: #fab387; font-weight: bold; border-radius: 8px; }
        button.suggested-action { background-color: #f38ba8; font-weight: bold; }
        progressbar progress { background-color: #fab387; border-radius: 25px; }
        label { font-family: 'Assistant', sans-serif; font-size: 14px; }
    "#);
    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().expect("Display error"),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Burn Station Pro 2026")
        .default_width(600)
        .build();
    
    let vbox = Box::new(Orientation::Vertical, 15);
    vbox.set_margin_top(30);
    vbox.set_margin_bottom(30);
    vbox.set_margin_start(30);
    vbox.set_margin_end(30);


    // 2. רכיבי הממשק
    let iso_label = Label::new(Some("No ISO selected"));
    let iso_btn = Button::with_label("SELECT ISO");
    let drive_combo = ComboBoxText::new();
    let scan_btn = Button::with_label("SCAN DEVICES");
    scan_btn.add_css_class("refresh-button");
    let progress_bar = ProgressBar::new();
    let status_label = Label::new(Some("Ready to Create Magic."));
    let start_btn = Button::with_label("START BURNING");
    start_btn.add_css_class("suggested-action");

    // חיבור כפתור ה-SCAN לפונקציית הסריקה
    let drive_combo_clone = drive_combo.clone();
    scan_btn.connect_clicked(move |_| {
        update_device_list(&drive_combo_clone);
    });

    // סריקה ראשונית אוטומטית כשהתוכנה נדלקת
    update_device_list(&drive_combo);

    // סידור על המסך
    vbox.append(&iso_btn);
    vbox.append(&iso_label);
    vbox.append(&scan_btn);
    vbox.append(&drive_combo);
    vbox.append(&progress_bar);
    vbox.append(&status_label);
    vbox.append(&start_btn);
    window.set_child(Some(&vbox));

    // 3. חיבור המנוע והעברת הודעות (The Bridge)
    let engine = Arc::new(BurnEngine::new());
    let (sender, receiver) = gtk4::glib::MainContext::channel::<BurnEvent>(gtk4::glib::Priority::DEFAULT);
    
    // --- התיקון הקריטי: הגישור ---
    // פותחים חוט ברקע שלוקח מהמנוע ודוחף ל-UI בזמן אמת
    let event_rx = engine.event_rx.clone();
    std::thread::spawn(move || {
        while let Ok(event) = event_rx.recv() {
            if sender.send(event).is_err() {
                break; // החלון נסגר, מפסיקים להעביר
            }
        }
    });
    let iso_path = Arc::new(RefCell::new(None::<PathBuf>));
    // עדכון ה-UI כשהמנוע שולח הודעה
    let progress_clone = progress_bar.clone();
    let status_clone = status_label.clone();
    receiver.attach(None, move |event| {
        match event {
            BurnEvent::Progress {
                written,
                total,
                speed_mbps,
            } => {
                let fraction = written as f64 / total as f64;
                progress_clone.set_fraction(fraction);
                status_clone.set_text(&format!(
                    "{:.1} MB/s | {}%",
                    speed_mbps,
                    (fraction * 100.0) as u64
                ));
            }
            BurnEvent::Finished => {
                status_clone.set_text("Success! Drive is ready.");
                progress_clone.set_fraction(1.0);
            }
            BurnEvent::Error(e) => {
                status_clone.set_text(&format!("Error: {}", e));
            }
            _ => {}
        }
        gtk4::glib::ControlFlow::Continue
    });

    // כפתור בחירת ISO
    let iso_label_c = iso_label.clone();
    let iso_path_c = iso_path.clone();
    iso_btn.connect_clicked(move |_| {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("ISO", &["iso"])
            .pick_file()
        {
            iso_label_c.set_text(&path.display().to_string());
            *iso_path_c.borrow_mut() = Some(path);
        }
    });

    // כפתור התחלה
    let engine_c = engine.clone();
    let drive_c = drive_combo.clone();
    start_btn.connect_clicked(move |_| {




        if let (Some(iso), Some(dev)) = (iso_path.borrow().clone(), drive_c.active_id()) {
            engine_c.start(BurnConfig {
                iso_path: iso,
                device_path: PathBuf::from(dev.as_str()),
                verify: true,
            });
        }
    });

    window.present();
}

fn update_device_list(combo: &gtk4::ComboBoxText) {
    combo.remove_all();
    
    // הרצה של lsblk עם הגדרות רחבות יותר כדי לוודא שזה מוצא משהו
    let output = std::process::Command::new("lsblk")
        .args(["-dpno", "NAME,SIZE,MODEL"])
        .output();

    let mut found = false;

    if let Ok(out) = output {
        let list = String::from_utf8_lossy(&out.stdout);
        println!("Scanning drives: \n{}", list); // הדפסה לטרמינל לדיבוג

        for line in list.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let dev_path = parts[0];
                let dev_info = line.trim();
                
                // אנחנו מסננים רק כוננים שלמים (בלי מחיצות כמו sda1)
                if !dev_path.chars().last().unwrap_or(' ').is_numeric() {
                    combo.append(Some(dev_path), dev_info);
                    found = true;
                }
            }
        }
    }

    if !found {
        println!("No USB drives found!");
        combo.append(Some("none"), "No drives detected - Click SCAN");
    }
    
    combo.set_active(Some(0));
}