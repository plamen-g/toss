//! The window: your devices, what's happening right now, an Accept/Decline card for incoming files,
//! and an "Add device" page for pairing. It keeps receiving from the tray after the window closes.

use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Result;
use slint::{Color, ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::sync::oneshot;

use crate::{Answer, Event, Ui, count, human};

slint::slint! {
    import { ListView, Palette } from "std-widgets.slint";

    export struct Device { name: string, initial: string, tint: color, auto: bool }

    // Colors from the logo: deep navy with a cyan glow.
    global Theme {
        out property <color> bg-top: #102349;
        out property <color> bg-bottom: #091429;
        out property <color> surface: #ffffff0f;
        out property <color> surface-hover: #ffffff1c;
        out property <color> line: #ffffff1c;
        out property <color> text: #eaf2ff;
        out property <color> muted: #93a6c9;
        out property <color> accent: #22d3ee;
        out property <color> accent-hover: #67e8f9;
        out property <color> accent-pressed: #06b6d4;
        out property <color> on-accent: #062a33;
        out property <color> ok: #34d399;
        out property <color> bad: #fb7185;
    }

    // Icon outlines on a 24x24 grid.
    global Icons {
        out property <string> plus: "M12 5v14M5 12h14";
        out property <string> up: "M12 19V5M5 12l7-7 7 7";
        out property <string> down: "M12 5v14M19 12l-7 7-7-7";
        out property <string> check: "M20 6L9 17l-5-5";
        out property <string> alert: "M12 7v6M12 16v1";
        out property <string> folder: "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z";
        out property <string> back: "M19 12H5M12 19l-7-7 7-7";
        out property <string> trash: "M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3";
        out property <string> link: "M10 14a4 4 0 0 0 6 0l3-3a4 4 0 0 0-6-6l-1 1M14 10a4 4 0 0 0-6 0l-3 3a4 4 0 0 0 6 6l1-1";
    }

    component Icon inherits Path {
        width: 18px;
        height: 18px;
        viewbox-width: 24;
        viewbox-height: 24;
        fill: transparent;
        stroke: Theme.text;
        stroke-width: 2px;
        stroke-line-cap: round;
        accessible-role: none;
    }

    component Badge inherits Rectangle {
        in property <string> icon;
        in property <color> tint: Theme.accent;
        width: 40px;
        height: 40px;
        border-radius: 20px;
        background: root.tint.transparentize(0.82);
        Icon { x: 11px; y: 11px; commands: root.icon; stroke: root.tint; }
    }

    component Avatar inherits Rectangle {
        in property <string> letter;
        in property <color> tint;
        width: 44px;
        height: 44px;
        border-radius: 22px;
        background: root.tint;
        Text {
            text: root.letter;
            color: white;
            font-size: 18px;
            font-weight: 700;
            horizontal-alignment: center;
            vertical-alignment: center;
        }
    }

    component Card inherits Rectangle {
        in property <bool> highlight;
        vertical-stretch: 0; // spare height belongs to the device list
        border-radius: 16px;
        border-width: 1px;
        border-color: root.highlight ? Theme.accent.transparentize(0.45) : Theme.line;
        background: root.highlight ? Theme.accent.transparentize(0.9) : Theme.surface;
    }

    // Our own buttons, so toss looks the same everywhere; they keep keyboard and screen-reader support.
    component PillButton inherits Rectangle {
        in property <string> text;
        in property <string> label; // for screen readers when the text alone isn't enough
        in property <string> icon;
        in property <bool> primary;
        in property <bool> danger;
        in property <bool> enabled: true;
        callback clicked;
        private property <color> ink: !root.enabled ? Theme.muted.transparentize(0.35)
            : root.primary ? Theme.on-accent : root.danger ? Theme.bad : Theme.text;

        accessible-role: button;
        accessible-label: root.label != "" ? root.label : root.text;
        accessible-enabled: root.enabled;
        accessible-action-default => { if (root.enabled) { root.clicked(); } }

        height: 36px;
        horizontal-stretch: 0; // buttons keep their natural width
        border-radius: self.height / 2;
        border-width: focus.has-focus ? 2px : (root.primary && root.enabled ? 0px : 1px);
        border-color: focus.has-focus ? Theme.accent-hover : root.danger ? Theme.bad.transparentize(0.5) : Theme.line;
        background: !root.enabled ? Theme.surface
            : root.primary ? (touch.pressed ? Theme.accent-pressed : touch.has-hover ? Theme.accent-hover : Theme.accent)
            : root.danger ? (touch.has-hover ? Theme.bad.transparentize(0.72) : Theme.bad.transparentize(0.85))
            : (touch.pressed ? #ffffff2e : touch.has-hover ? Theme.surface-hover : Theme.surface);

        HorizontalLayout {
            padding-left: root.icon != "" ? 12px : 16px;
            padding-right: 16px;
            spacing: 6px;
            alignment: center;
            if root.icon != "": VerticalLayout {
                alignment: center;
                Icon {
                    width: 16px;
                    height: 16px;
                    commands: root.icon;
                    stroke: root.ink;
                }
            }
            Text {
                text: root.text;
                color: root.ink;
                font-weight: 600;
                vertical-alignment: center;
            }
        }
        touch := TouchArea {
            enabled: root.enabled;
            mouse-cursor: pointer;
            clicked => { root.clicked(); }
        }
        focus := FocusScope {
            enabled: root.enabled;
            focus-on-click: false;
            key-pressed(event) => {
                if (event.text == " " || event.text == "\n") {
                    root.clicked();
                    return accept;
                }
                reject
            }
        }
    }

    component IconButton inherits Rectangle {
        in property <string> icon; // "" draws the "more" dots (the renderer can't draw dot-shaped strokes)
        in property <string> label;
        callback clicked;
        accessible-role: button;
        accessible-label: root.label;
        accessible-action-default => { root.clicked(); }
        width: 36px;
        height: 36px;
        border-radius: 18px;
        border-width: focus.has-focus ? 2px : 0px;
        border-color: Theme.accent-hover;
        background: touch.pressed ? #ffffff2e : touch.has-hover ? Theme.surface-hover : transparent;
        if root.icon != "": Icon { x: 9px; y: 9px; commands: root.icon; }
        if root.icon == "": HorizontalLayout {
            alignment: center;
            spacing: 3px;
            for i in 3: VerticalLayout {
                alignment: center;
                Rectangle { width: 4px; height: 4px; border-radius: 2px; background: Theme.text; }
            }
        }
        touch := TouchArea { mouse-cursor: pointer; clicked => { root.clicked(); } }
        focus := FocusScope {
            focus-on-click: false;
            key-pressed(event) => {
                if (event.text == " " || event.text == "\n") {
                    root.clicked();
                    return accept;
                }
                reject
            }
        }
    }

    component MenuRow inherits Rectangle {
        in property <string> text;
        in property <string> icon;
        in property <bool> danger;
        callback clicked;
        accessible-role: button;
        accessible-label: root.text;
        accessible-action-default => { root.clicked(); }
        height: 38px;
        border-radius: 8px;
        background: touch.has-hover || focus.has-focus ? Theme.surface-hover : transparent;
        HorizontalLayout {
            padding-left: 10px;
            padding-right: 10px;
            spacing: 10px;
            VerticalLayout {
                alignment: center;
                Icon { width: 16px; height: 16px; commands: root.icon; stroke: root.danger ? Theme.bad : Theme.text; }
            }
            Text { text: root.text; color: root.danger ? Theme.bad : Theme.text; vertical-alignment: center; horizontal-stretch: 1; }
        }
        touch := TouchArea { mouse-cursor: pointer; clicked => { root.clicked(); } }
        focus := FocusScope {
            focus-on-click: false;
            key-pressed(event) => {
                if (event.text == " " || event.text == "\n") {
                    root.clicked();
                    return accept;
                }
                reject
            }
        }
    }

    component Field inherits Rectangle {
        in property <string> placeholder;
        in property <string> label;
        in-out property <string> text <=> input.text;
        height: 36px;
        min-width: 0;
        horizontal-stretch: 1;
        border-radius: 10px;
        background: #00000033;
        border-width: input.has-focus ? 2px : 1px;
        border-color: input.has-focus ? Theme.accent : Theme.line;
        input := TextInput {
            x: 12px;
            width: parent.width - 24px;
            height: parent.height;
            vertical-alignment: center;
            single-line: true;
            color: Theme.text;
            selection-background-color: Theme.accent.transparentize(0.55);
            accessible-label: root.label;
            accessible-placeholder-text: root.placeholder;
        }
        if input.text == "": Text {
            x: 12px;
            width: parent.width - 24px;
            height: parent.height;
            text: root.placeholder;
            color: Theme.muted.transparentize(0.25);
            vertical-alignment: center;
            overflow: elide;
            accessible-role: none;
        }
    }

    component StatusPill inherits Rectangle {
        accessible-role: text;
        accessible-label: "Not receiving";
        height: 26px;
        border-radius: 13px;
        background: Theme.bad.transparentize(0.86);
        HorizontalLayout {
            padding-left: 9px;
            padding-right: 11px;
            spacing: 6px;
            VerticalLayout {
                alignment: center;
                Rectangle { width: 7px; height: 7px; border-radius: 3.5px; background: Theme.bad; }
            }
            Text {
                text: "Not receiving";
                font-size: 12px;
                font-weight: 600;
                color: Theme.bad;
                vertical-alignment: center;
            }
        }
    }

    component Bar inherits Rectangle {
        in property <float> value;
        accessible-role: progress-indicator;
        accessible-value: round(root.value * 100) + "%";
        height: 6px;
        border-radius: 3px;
        background: #ffffff1f;
        Rectangle {
            x: 0;
            width: parent.width * clamp(root.value, 0, 1);
            border-radius: 3px;
            background: Theme.accent;
        }
    }

    component DeviceRow inherits Rectangle {
        in property <Device> device;
        callback send-files(string);
        callback send-folder(string);
        callback forget(string);
        property <bool> confirming;
        height: 78px;

        Card {
            y: 0;
            height: parent.height - 10px;
            HorizontalLayout {
                padding-left: 14px;
                padding-right: 10px;
                spacing: 12px;
                VerticalLayout { alignment: center; Avatar { letter: root.device.initial; tint: root.device.tint; } }
                VerticalLayout {
                    alignment: center;
                    spacing: 2px;
                    horizontal-stretch: 1;
                    Text { text: root.device.name; font-size: 15px; font-weight: 700; color: Theme.text; overflow: elide; }
                    if root.device.auto: Text { text: "Auto-accept"; font-size: 12px; color: Theme.muted; }
                }
                VerticalLayout {
                    alignment: center;
                    PillButton {
                        text: "Send";
                        label: "Send files to " + root.device.name;
                        icon: Icons.up;
                        primary: true;
                        clicked => { root.send-files(root.device.name); }
                    }
                }
                VerticalLayout {
                    alignment: center;
                    IconButton {
                        label: "More for " + root.device.name;
                        clicked => {
                            root.confirming = false;
                            menu.show();
                        }
                    }
                }
            }
        }

        menu := PopupWindow {
            x: root.width - self.width - 8px;
            y: 58px;
            width: 240px;
            close-policy: close-on-click-outside;
            Rectangle {
                border-radius: 12px;
                background: #14284f;
                border-width: 1px;
                border-color: Theme.line;
                drop-shadow-blur: 18px;
                drop-shadow-color: #00000088;
                VerticalLayout {
                    padding: 6px;
                    spacing: 2px;
                    if !root.confirming: MenuRow {
                        text: "Send a folder";
                        icon: Icons.folder;
                        clicked => {
                            menu.close();
                            root.send-folder(root.device.name);
                        }
                    }
                    if !root.confirming: MenuRow {
                        text: "Remove device";
                        icon: Icons.trash;
                        danger: true;
                        clicked => { root.confirming = true; }
                    }
                    if root.confirming: VerticalLayout {
                        padding: 8px;
                        spacing: 10px;
                        Text { text: "Remove " + root.device.name + "?"; font-weight: 700; color: Theme.text; }
                        Text {
                            text: "Deletes the pairing. Reconnecting needs a new invite link.";
                            font-size: 12px;
                            color: Theme.muted;
                            wrap: word-wrap;
                        }
                        HorizontalLayout {
                            spacing: 6px;
                            alignment: end;
                            PillButton { text: "Cancel"; clicked => { menu.close(); } }
                            PillButton {
                                text: "Remove";
                                danger: true;
                                clicked => {
                                    menu.close();
                                    root.forget(root.device.name);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    export component AppWindow inherits Window {
        title: "toss";
        icon: @image-url("assets/icon.png");
        preferred-width: 440px;
        preferred-height: 640px;
        min-width: 400px;
        min-height: 540px;
        default-font-size: 14px;
        background: @linear-gradient(180deg, Theme.bg-top 0%, Theme.bg-bottom 100%);

        // The brand is dark, so keep the built-in text fields dark even on a light system.
        init => { Palette.color-scheme = ColorScheme.dark; }

        in property <[Device]> devices;
        in property <bool> receiving: true;
        in property <int> act-kind; // 0 nothing yet (card hidden), 1 sending, 2 receiving, 3 done, 4 problem, 5 note
        in property <string> act-title;
        in property <string> act-detail;
        in property <bool> act-folder;
        in property <float> progress: -1; // below 0 hides the bar
        in property <string> offer-from; // incoming files waiting for an answer, "" when none
        in property <string> offer-initial;
        in property <color> offer-tint;
        in property <string> offer-summary;
        in property <string> offer-files;
        in property <string> link; // invite link to share, "" when none
        in property <string> pair-error;
        in property <bool> busy; // joining
        in-out property <bool> copied;
        in-out property <int> page; // 0 devices, 1 add device
        callback send-files(string);
        callback send-folder(string);
        callback forget(string);
        callback invite(string);
        callback join(string, string);
        callback answer(int); // 0 decline, 1 accept, 2 always
        callback open-downloads();

        private property <color> kind-color: root.act-kind == 3 ? Theme.ok : root.act-kind == 4 ? Theme.bad
            : root.act-kind == 5 ? Theme.muted : Theme.accent;
        private property <string> kind-icon: root.act-kind == 1 ? Icons.up : root.act-kind == 3 ? Icons.check
            : root.act-kind == 4 ? Icons.alert : Icons.down;

        VerticalLayout {
            padding: 20px;
            padding-top: 18px;
            spacing: 16px;

            if root.page == 0: HorizontalLayout {
                spacing: 10px;
                VerticalLayout {
                    alignment: center;
                    Image {
                        source: @image-url("assets/icon-64.png"); // pre-shrunk: the renderer scales down roughly
                        width: 34px;
                        height: 34px;
                        accessible-role: none;
                    }
                }
                VerticalLayout { alignment: center; horizontal-stretch: 0; Text { text: "toss"; font-size: 23px; font-weight: 800; color: Theme.text; } }
                if !root.receiving: VerticalLayout { alignment: center; horizontal-stretch: 0; StatusPill { } }
                Rectangle { horizontal-stretch: 1; }
                VerticalLayout {
                    alignment: center;
                    PillButton { text: "Add device"; icon: Icons.plus; clicked => { root.page = 1; } }
                }
            }
            if root.page == 1: HorizontalLayout {
                spacing: 8px;
                IconButton { icon: Icons.back; label: "Back"; clicked => { root.page = 0; } }
                VerticalLayout { alignment: center; Text { text: "Add device"; font-size: 20px; font-weight: 800; color: Theme.text; } }
                Rectangle { horizontal-stretch: 1; }
            }

            // Incoming files come first, whatever page is open.
            if root.offer-from != "": Card {
                highlight: true;
                VerticalLayout {
                    padding: 16px;
                    spacing: 14px;
                    HorizontalLayout {
                        spacing: 14px;
                        VerticalLayout { alignment: center; Avatar { letter: root.offer-initial; tint: root.offer-tint; } }
                        VerticalLayout {
                            alignment: center;
                            spacing: 3px;
                            horizontal-stretch: 1;
                            Text {
                                text: "Incoming from " + root.offer-from;
                                font-size: 16px;
                                font-weight: 700;
                                color: Theme.text;
                                overflow: elide;
                            }
                            Text { text: root.offer-summary; font-size: 12px; color: Theme.muted; }
                        }
                    }
                    Rectangle {
                        border-radius: 10px;
                        background: #00000033;
                        VerticalLayout {
                            padding: 12px;
                            Text { text: root.offer-files; font-size: 13px; color: Theme.text; wrap: word-wrap; }
                        }
                    }
                    HorizontalLayout {
                        spacing: 8px;
                        alignment: end;
                        PillButton { text: "Decline"; clicked => { root.answer(0); } }
                        PillButton { text: "Always accept"; label: "Always accept from " + root.offer-from; clicked => { root.answer(2); } }
                        PillButton { text: "Accept"; icon: Icons.down; primary: true; clicked => { root.answer(1); } }
                    }
                }
            }

            // What's happening right now.
            if root.page == 0 && root.offer-from == "" && root.act-kind != 0: Card {
                VerticalLayout {
                    padding: 16px;
                    spacing: 12px;
                    HorizontalLayout {
                        spacing: 14px;
                        VerticalLayout { alignment: center; Badge { icon: root.kind-icon; tint: root.kind-color; } }
                        VerticalLayout {
                            alignment: center;
                            spacing: 3px;
                            horizontal-stretch: 1;
                            Text { text: root.act-title; font-size: 16px; font-weight: 700; color: Theme.text; overflow: elide; }
                            Text { text: root.act-detail; font-size: 12px; color: Theme.muted; wrap: word-wrap; }
                        }
                        if root.act-folder: VerticalLayout {
                            alignment: center;
                            PillButton {
                                text: "Show";
                                label: "Open the Downloads folder";
                                icon: Icons.folder;
                                clicked => { root.open-downloads(); }
                            }
                        }
                    }
                    if root.progress >= 0: Bar { value: root.progress; }
                }
            }

            if root.page == 0: HorizontalLayout {
                Text { text: "Devices"; font-size: 13px; font-weight: 700; color: Theme.muted; }
                Rectangle { horizontal-stretch: 1; }
                if root.devices.length > 0: Text { text: root.devices.length; font-size: 13px; color: Theme.muted; }
            }
            if root.page == 0 && root.devices.length == 0: Card {
                VerticalLayout {
                    padding: 24px;
                    spacing: 10px;
                    HorizontalLayout {
                        alignment: center;
                        Image { source: @image-url("assets/icon-64.png"); width: 56px; height: 56px; accessible-role: none; }
                    }
                    Text { text: "No devices"; font-size: 17px; font-weight: 700; color: Theme.text; horizontal-alignment: center; }
                    Text {
                        text: "Paired devices appear here.";
                        font-size: 13px;
                        color: Theme.muted;
                        wrap: word-wrap;
                        horizontal-alignment: center;
                    }
                    HorizontalLayout {
                        alignment: center;
                        PillButton { text: "Add device"; icon: Icons.plus; primary: true; clicked => { root.page = 1; } }
                    }
                }
            }
            if root.page == 0 && root.devices.length > 0: ListView {
                vertical-stretch: 1;
                for device in root.devices: DeviceRow {
                    device: device;
                    send-files(name) => { root.send-files(name); }
                    send-folder(name) => { root.send-folder(name); }
                    forget(name) => { root.forget(name); }
                }
            }

            if root.page == 1: Card {
                VerticalLayout {
                    padding: 16px;
                    spacing: 12px;
                    HorizontalLayout {
                        spacing: 12px;
                        VerticalLayout { alignment: center; Badge { icon: Icons.link; } }
                        VerticalLayout {
                            alignment: center;
                            spacing: 2px;
                            Text { text: "Invite link"; font-size: 15px; font-weight: 700; color: Theme.text; }
                            Text { text: "One-time pairing link. Valid while toss is running."; font-size: 12px; color: Theme.muted; wrap: word-wrap; }
                        }
                    }
                    HorizontalLayout {
                        spacing: 8px;
                        invite-name := Field { placeholder: "Device name"; label: "Name for the new device"; }
                        PillButton {
                            text: "Create link";
                            primary: true;
                            enabled: invite-name.text != "" && root.link == "";
                            clicked => { root.invite(invite-name.text); }
                        }
                    }
                    if root.link != "": VerticalLayout {
                        spacing: 10px;
                        HorizontalLayout {
                            spacing: 8px;
                            Rectangle {
                                horizontal-stretch: 1;
                                min-width: 0;
                                height: 36px;
                                border-radius: 10px;
                                background: #00000033;
                                border-width: 1px;
                                border-color: Theme.line;
                                link-text := TextInput {
                                    x: 12px;
                                    width: parent.width - 24px;
                                    height: parent.height;
                                    text: root.link;
                                    read-only: true;
                                    single-line: true;
                                    vertical-alignment: center;
                                    font-size: 12px;
                                    color: Theme.text;
                                    accessible-label: "Your invite link";
                                }
                            }
                            PillButton {
                                text: root.copied ? "Copied" : "Copy";
                                icon: root.copied ? Icons.check : "";
                                clicked => {
                                    link-text.select-all();
                                    link-text.copy();
                                    root.copied = true;
                                }
                            }
                        }
                        Text { text: "Waiting for the other device"; font-size: 12px; color: Theme.muted; }
                    }
                }
            }
            if root.page == 1: Card {
                VerticalLayout {
                    padding: 16px;
                    spacing: 12px;
                    HorizontalLayout {
                        spacing: 12px;
                        VerticalLayout { alignment: center; Badge { icon: Icons.down; } }
                        VerticalLayout {
                            alignment: center;
                            spacing: 2px;
                            Text { text: "Join with link"; font-size: 15px; font-weight: 700; color: Theme.text; }
                            Text { text: "Pairs with the device that created the link."; font-size: 12px; color: Theme.muted; wrap: word-wrap; }
                        }
                    }
                    join-link := Field { placeholder: "Invite link"; label: "Invite link"; }
                    HorizontalLayout {
                        spacing: 8px;
                        join-name := Field { placeholder: "Device name"; label: "Name for the inviting device"; }
                        PillButton {
                            text: root.busy ? "Joining…" : "Join";
                            primary: true;
                            enabled: !root.busy && join-link.text != "" && join-name.text != "";
                            clicked => { root.join(join-link.text, join-name.text); }
                        }
                    }
                }
            }
            if root.page == 1 && root.pair-error != "": Text { text: root.pair-error; color: Theme.bad; font-size: 13px; wrap: word-wrap; }
            if root.page == 1 || (root.page == 0 && root.devices.length == 0): Rectangle { vertical-stretch: 1; }
        }
    }

    // Lives in the notification area / menu bar while the window is closed.
    export component Tray inherits SystemTrayIcon {
        icon: @image-url("assets/icon.png");
        tooltip: "toss";
        callback open();
        callback quit();
        clicked => { root.open(); }
        Menu {
            MenuItem { title: "Open toss"; activated => { root.open(); } }
            MenuItem { title: "Quit"; activated => { root.quit(); } }
        }
    }
}

// What the card at the top shows (act-kind in the markup above).
const SENDING: i32 = 1;
const RECEIVING: i32 = 2;
const DONE: i32 = 3;
const PROBLEM: i32 = 4;
const NOTE: i32 = 5;

struct Gui {
    win: slint::Weak<AppWindow>,
    home: PathBuf,
    pending: Mutex<Option<oneshot::Sender<Answer>>>, // the Accept/Decline card waiting for a click
    transfer: Mutex<String>,                         // headline while bytes move, e.g. "Sending to laptop"
}

impl Gui {
    fn update(&self, change: impl FnOnce(AppWindow) + Send + 'static) {
        let _ = self.win.upgrade_in_event_loop(change);
    }

    /// The card at the top: what's happening right now.
    fn show(&self, kind: i32, title: String, detail: String, folder: bool) {
        self.update(move |w| {
            w.set_act_kind(kind);
            w.set_act_title(title.into());
            w.set_act_detail(detail.into());
            w.set_act_folder(folder);
            w.set_progress(-1.0);
        });
    }

    fn refresh_devices(&self) {
        let contacts = crate::load_contacts(&self.home).unwrap_or_default();
        let devices: Vec<(String, bool)> = contacts.into_iter().map(|(name, c)| (name, c.auto_accept)).collect();
        // Model types aren't Send, so build them on the UI thread.
        self.update(move |w| {
            let devices: Vec<Device> = devices
                .into_iter()
                .map(|(name, auto)| Device { initial: initial(&name), tint: tint(&name), name: name.into(), auto })
                .collect();
            w.set_devices(ModelRc::new(VecModel::from(devices)));
        });
    }
}

/// " · resumed" when a transfer continued an earlier try (the terminal prints the exact amount).
fn resumed_note(resumed: u64) -> &'static str {
    if resumed > 0 { " · resumed" } else { "" }
}

fn initial(name: &str) -> SharedString {
    name.chars().next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default().into()
}

/// A stable color per name, so a device keeps its color as others are added.
fn tint(name: &str) -> Color {
    const TINTS: [(u8, u8, u8); 6] =
        [(0xf9, 0x73, 0x16), (0xa7, 0x8b, 0xfa), (0x34, 0xd3, 0x99), (0xf4, 0x72, 0xb6), (0x60, 0xa5, 0xfa), (0xfb, 0xbf, 0x24)];
    let (r, g, b) = TINTS[name.bytes().map(usize::from).sum::<usize>() % TINTS.len()];
    Color::from_rgb_u8(r, g, b)
}

impl Ui for Gui {
    fn event(&self, event: Event) {
        match event {
            Event::Ready(_) => self.update(|w| w.set_receiving(true)),
            Event::Waiting { to, files } => {
                *self.transfer.lock().expect("not poisoned") = format!("Sending to {to}");
                self.show(SENDING, format!("Sending to {to}"), format!("Waiting for approval · {}", count(files)), false);
            }
            Event::Sent { to, files, direct, mb_per_s, resumed } => {
                let route = if direct { "direct" } else { "relayed" };
                let speed = mb_per_s.map(|s| format!(" · {s:.1} MB/s")).unwrap_or_default();
                let detail = format!("{} · {route}{speed}{}", count(files), resumed_note(resumed));
                self.show(DONE, format!("Sent to {to}"), detail, false);
            }
            Event::Receiving { from, files } => {
                *self.transfer.lock().expect("not poisoned") = format!("Receiving from {from}");
                self.show(RECEIVING, format!("Receiving from {from}"), count(files), false);
            }
            Event::Checking { bytes } => {
                let detail = format!("Checking {} from an earlier try", human(bytes));
                self.update(move |w| w.set_act_detail(detail.into()));
            }
            Event::Received { from, files, resumed } => {
                let detail = format!("{} · Downloads{}", count(files), resumed_note(resumed));
                self.show(DONE, format!("Received from {from}"), detail, true)
            }
            Event::Declined { from } => self.show(NOTE, "Transfer declined".into(), format!("From {from}"), false),
            Event::Failed(why) => self.show(PROBLEM, "Transfer failed".into(), why, false),
        }
        self.refresh_devices(); // pairings and "always" answers change contacts; events are rare
    }

    // shortcut: transfers running at the same time share this one bar; give each its own row if that gets common.
    fn progress(&self, done: u64, total: u64) {
        let title = self.transfer.lock().expect("not poisoned").clone();
        let p = if done >= total { -1.0 } else { done as f32 / total as f32 };
        let detail = format!("{} / {}", human(done), human(total));
        self.update(move |w| {
            if p >= 0.0 {
                w.set_act_title(title.into());
                w.set_act_detail(detail.into());
            }
            w.set_progress(p);
        });
    }

    fn accept(&self, from: &str, files: &[(String, u64)]) -> impl Future<Output = Answer> + Send {
        let total: u64 = files.iter().map(|f| f.1).sum();
        let summary = format!("{} · {}", count(files.len()), human(total));
        let mut names = files.iter().take(4).map(|f| f.0.as_str()).collect::<Vec<_>>().join("\n");
        if files.len() > 4 {
            names += &format!("\nand {} more", files.len() - 4);
        }
        let (tx, rx) = oneshot::channel();
        *self.pending.lock().expect("not poisoned") = Some(tx);
        let from = from.to_string();
        let (who, what) = (from.clone(), summary.clone());
        self.update(move |w| {
            let _ = w.show(); // it may be closed to the tray: the question has to be seen
            w.window().set_minimized(false);
            w.set_offer_tint(tint(&who));
            w.set_offer_initial(initial(&who));
            w.set_offer_summary(what.into());
            w.set_offer_files(names.into());
            w.set_offer_from(who.into());
        });
        async move {
            let answer = rx.await.unwrap_or(Answer::No);
            self.update(|w| w.set_offer_from("".into()));
            if !matches!(answer, Answer::No) {
                *self.transfer.lock().expect("not poisoned") = format!("Receiving from {from}");
                self.show(RECEIVING, format!("Receiving from {from}"), summary, false);
            }
            answer
        }
    }
}

pub fn run() -> Result<()> {
    hide_console();
    let home = crate::home()?;
    // shortcut: the probe lock is released before listen() takes it, so two copies started at the
    // same instant could both pass; harmless, the second's listen() then reports the clash.
    if crate::lock_listener(&home).is_err() {
        rfd::MessageDialog::new()
            .set_title("toss")
            .set_description("toss is already running. Open it from its icon in the system tray (near the clock).")
            .show();
        return Ok(());
    }
    let win = AppWindow::new()?;
    let gui = Arc::new(Gui { win: win.as_weak(), home, pending: Mutex::new(None), transfer: Mutex::default() });
    gui.refresh_devices();

    let g = gui.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::listen(g.clone()).await {
            g.update(|w| w.set_receiving(false));
            g.show(PROBLEM, "Not receiving".into(), format!("{e:#}"), false);
        }
    });

    win.on_send_files(sender(&gui, |dialog| dialog.pick_files()));
    win.on_send_folder(sender(&gui, |dialog| dialog.pick_folder().map(|folder| vec![folder])));
    win.on_open_downloads(|| {
        if let Ok(dir) = crate::downloads_dir() {
            open_folder(&dir);
        }
    });

    let g = gui.clone();
    win.on_forget(move |name| {
        match crate::forget(&name) {
            Ok(()) => g.show(NOTE, format!("Removed {name}"), "Pairing deleted".into(), false),
            Err(e) => g.show(PROBLEM, format!("Removing {name} failed"), format!("{e:#}"), false),
        }
        g.refresh_devices();
    });

    let g = gui.clone();
    win.on_invite(move |name| {
        let g = g.clone();
        g.update(|w| w.set_pair_error("".into()));
        tokio::spawn(async move {
            let name = name.trim().to_string();
            let (link, joined) = match crate::invite(&name).await {
                Ok(started) => started,
                Err(e) => return g.update(move |w| w.set_pair_error(format!("{e:#}").into())),
            };
            g.update(move |w| {
                w.set_copied(false);
                w.set_link(link.into());
            });
            let res = joined.await;
            g.update(|w| w.set_link("".into()));
            match res {
                Ok(()) => paired(&g, &name),
                Err(e) => g.update(move |w| w.set_pair_error(format!("{e:#}").into())),
            }
        });
    });

    let g = gui.clone();
    win.on_join(move |link, name| {
        let g = g.clone();
        g.update(|w| {
            w.set_pair_error("".into());
            w.set_busy(true);
        });
        tokio::spawn(async move {
            let name = name.trim().to_string();
            let res = crate::join(&link, &name).await;
            g.update(|w| w.set_busy(false));
            match res {
                Ok(()) => paired(&g, &name),
                Err(e) => g.update(move |w| w.set_pair_error(format!("{e:#}").into())),
            }
        });
    });

    let g = gui.clone();
    win.on_answer(move |answer| {
        if let Some(tx) = g.pending.lock().expect("not poisoned").take() {
            let _ = tx.send(match answer {
                1 => Answer::Yes,
                2 => Answer::Always,
                _ => Answer::No,
            });
        }
    });

    let tray = Tray::new()?;
    let w = win.as_weak();
    tray.on_open(move || {
        if let Some(w) = w.upgrade() {
            let _ = w.show();
            w.window().set_minimized(false);
        }
    });
    tray.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // Closing the window only hides it (Slint's default); toss keeps receiving until Quit.
    win.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(())
}

fn paired(gui: &Gui, name: &str) {
    gui.show(DONE, format!("Paired with {name}"), "Added to devices".into(), false);
    gui.refresh_devices();
    gui.update(|w| w.set_page(0));
}

/// A Send button handler: pick what to send, then send it in the background.
fn sender(gui: &Arc<Gui>, pick: fn(rfd::FileDialog) -> Option<Vec<PathBuf>>) -> impl Fn(SharedString) + 'static {
    let gui = gui.clone();
    move |name| {
        let Some(paths) = pick(rfd::FileDialog::new()) else { return };
        let gui = gui.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::send(&name, &paths, &*gui).await {
                gui.show(PROBLEM, format!("Sending to {name} failed"), format!("{e:#}"), false);
            }
        });
    }
}

fn open_folder(dir: &Path) {
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(dir).spawn();
}

/// Started from Explorer, Windows gives a console program a console window; close it.
#[cfg(windows)]
fn hide_console() {
    unsafe extern "system" {
        safe fn FreeConsole() -> i32;
    }
    // shortcut: the console still flashes for a moment; ship a separate windows-subsystem binary with the installers.
    FreeConsole();
}

#[cfg(not(windows))]
fn hide_console() {}
