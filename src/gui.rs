//! The window: your devices, what's happening right now, an Accept/Decline card for incoming files,
//! an "Add device" page for pairing, and a "Phone" page with a QR code for a phone's browser. It keeps
//! receiving from the tray after the window closes.

use std::{
    future::Future,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use slint::{Color, ComponentHandle, Image, ModelRc, Rgb8Pixel, SharedPixelBuffer, SharedString, VecModel};
use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
use tokio::sync::oneshot;

use crate::{Answer, Event, Ui, count, human};

slint::slint! {
    import { ListView, Palette } from "std-widgets.slint";
    import "assets/BricolageGrotesque-Bold.ttf"; // the wordmark and page titles; names stay in the system font

    export struct Device { name: string, initial: string, tint: color, auto: bool }

    // Colors from the logo: deep navy with a cyan glow. Cyan is for files leaving, amber for files arriving.
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
        out property <color> catch: #fbbf24;
        out property <color> catch-hover: #fcd34d;
        out property <color> catch-pressed: #f59e0b;
        out property <color> on-catch: #3b2300;
        out property <string> display-font: "Bricolage Grotesque 96pt";
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
        out property <string> phone: "M8 2h8a2 2 0 0 1 2 2v16a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2zM11 18h2";
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

    // A document with a folded corner: the thing being tossed.
    component FileGlyph inherits Path {
        width: 14px;
        height: 18px;
        viewbox-width: 14;
        viewbox-height: 18;
        commands: "M1 1h8l4 4v12H1z";
        stroke-width: 0px;
        accessible-role: none;
    }

    // 5: a file dropping into place; plays once, when the card for a caught transfer appears.
    component CaughtBadge inherits Rectangle {
        property <float> fall: 0;
        animate fall { duration: 700ms; easing: ease-out-bounce; }
        init => { root.fall = 1; }
        width: 40px;
        height: 40px;
        border-radius: 20px;
        background: Theme.catch.transparentize(0.82);
        FileGlyph { x: 13px; y: -20px + 31px * root.fall; fill: Theme.catch; }
    }

    // 1 and 5: a transfer flies along an arc from the sender (left) to the receiver (right). The file
    // is tossed up into the air as the card appears, then rides the arc with the progress.
    component Flight inherits Rectangle {
        in property <float> progress; // 0 to 1
        in property <color> ink;
        in property <bool> incoming; // the other device is the sender, so it sits on the left
        in property <string> peer-initial;
        in property <color> peer-tint;
        property <float> shown: root.progress;
        property <float> thrown: 0;
        property <length> span: root.width - 48px;
        animate shown { duration: 250ms; } // progress arrives 4 times a second; glide between
        animate thrown { duration: 500ms; easing: ease-out-back; }
        init => { root.thrown = 1; }
        height: 64px;
        accessible-role: none;

        for i in 15: Rectangle {
            property <float> t: (i + 1) / 16.0;
            x: 24px + root.span * self.t - 2px;
            y: 52px - 160px * self.t * (1 - self.t) - 2px;
            width: 4px;
            height: 4px;
            border-radius: 2px;
            background: self.t <= root.shown ? root.ink : #ffffff26;
        }
        Rectangle { // you
            x: (root.incoming ? root.width - 24px : 24px) - 6px;
            y: 46px;
            width: 12px;
            height: 12px;
            border-radius: 6px;
            background: root.ink;
        }
        Rectangle { // them
            x: (root.incoming ? 24px : root.width - 24px) - 14px;
            y: 38px;
            width: 28px;
            height: 28px;
            border-radius: 14px;
            background: root.peer-tint;
            Text {
                text: root.peer-initial;
                color: white;
                font-size: 13px;
                font-weight: 700;
                horizontal-alignment: center;
                vertical-alignment: center;
            }
        }
        FileGlyph {
            x: 24px + root.span * root.shown - 7px;
            y: 52px - 160px * root.shown * (1 - root.shown) - 9px + (1 - root.thrown) * 22px;
            fill: root.ink;
        }
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
        in property <color> tint: Theme.accent; // the highlight's color
        vertical-stretch: 0; // spare height belongs to the device list
        border-radius: 16px;
        border-width: 1px;
        border-color: root.highlight ? root.tint.transparentize(0.45) : Theme.line;
        background: root.highlight ? root.tint.transparentize(0.9) : Theme.surface;
    }

    // Our own buttons, so toss looks the same everywhere; they keep keyboard and screen-reader support.
    component PillButton inherits Rectangle {
        in property <string> text;
        in property <string> label; // for screen readers when the text alone isn't enough
        in property <string> icon;
        in property <bool> primary;
        in property <bool> danger;
        in property <bool> warm; // amber instead of cyan: for files arriving
        in property <bool> enabled: true;
        callback clicked;
        private property <color> ink: !root.enabled ? Theme.muted.transparentize(0.35)
            : root.primary ? (root.warm ? Theme.on-catch : Theme.on-accent) : root.danger ? Theme.bad : Theme.text;
        private property <color> fill: root.warm ? Theme.catch : Theme.accent;
        private property <color> fill-hover: root.warm ? Theme.catch-hover : Theme.accent-hover;
        private property <color> fill-pressed: root.warm ? Theme.catch-pressed : Theme.accent-pressed;

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
            : root.primary ? (touch.pressed ? root.fill-pressed : touch.has-hover ? root.fill-hover : root.fill)
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
        in property <color> fill: Theme.accent;
        accessible-role: progress-indicator;
        accessible-value: round(root.value * 100) + "%";
        height: 6px;
        border-radius: 3px;
        background: #ffffff1f;
        Rectangle {
            x: 0;
            width: parent.width * clamp(root.value, 0, 1);
            border-radius: 3px;
            background: root.fill;
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
                        text: "Toss";
                        label: "Toss files to " + root.device.name;
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
                        text: "Toss a folder";
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
        in property <int> act-kind; // 0 nothing yet (card hidden), 1 tossing, 2 catching, 3 done, 4 problem, 5 note, 6 caught
        in property <string> act-peer-initial; // the other end of the flight arc
        in property <color> act-peer-tint;
        in property <string> act-title;
        in property <string> act-detail;
        in property <bool> act-folder; // the card is about received files: offer to show them
        in property <bool> act-open; // ...and it's a single file, so it can be opened
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
        in-out property <int> page; // 0 devices, 1 add device, 2 phone
        in property <image> phone-qr;
        in property <string> phone-url; // "" while the phone page's server isn't running
        in property <string> phone-offered; // names of the files the phone can download
        in property <string> phone-error;
        in property <string> phone-peer; // the device the phone page belongs to
        in property <bool> drop-hover; // files are being dragged over the window
        in property <int> dropped; // dropped items waiting for a device's Toss button
        callback drop-cancel();
        callback phone-open();
        callback phone-offer();
        callback phone-close();
        callback send-files(string);
        callback send-folder(string);
        callback forget(string);
        callback invite(string);
        callback join(string, string);
        callback answer(int); // 0 decline, 1 accept, 2 always
        callback open-saved();
        callback show-saved();

        private property <color> kind-color: root.act-kind == 2 || root.act-kind == 6 ? Theme.catch
            : root.act-kind == 4 ? Theme.bad : root.act-kind == 5 ? Theme.muted : Theme.accent;
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
                VerticalLayout { alignment: center; horizontal-stretch: 0; Text { text: "toss"; font-family: Theme.display-font; font-size: 27px; font-weight: 700; color: Theme.text; } }
                if !root.receiving: VerticalLayout { alignment: center; horizontal-stretch: 0; StatusPill { } }
                Rectangle { horizontal-stretch: 1; }
                VerticalLayout {
                    alignment: center;
                    PillButton {
                        text: "Phone";
                        label: "Send to or from a phone's browser";
                        icon: Icons.phone;
                        clicked => {
                            root.page = 2;
                            root.phone-open();
                        }
                    }
                }
                VerticalLayout {
                    alignment: center;
                    PillButton { text: "Add device"; icon: Icons.plus; clicked => { root.page = 1; } }
                }
            }
            if root.page == 1: HorizontalLayout {
                spacing: 8px;
                IconButton { icon: Icons.back; label: "Back"; clicked => { root.page = 0; } }
                VerticalLayout { alignment: center; Text { text: "Add device"; font-family: Theme.display-font; font-size: 22px; font-weight: 700; color: Theme.text; } }
                Rectangle { horizontal-stretch: 1; }
            }
            if root.page == 2: HorizontalLayout {
                spacing: 8px;
                IconButton {
                    icon: Icons.back;
                    label: "Back";
                    clicked => {
                        root.page = 0;
                        root.phone-close();
                    }
                }
                VerticalLayout { alignment: center; Text { text: "Phone"; font-family: Theme.display-font; font-size: 22px; font-weight: 700; color: Theme.text; } }
                Rectangle { horizontal-stretch: 1; }
            }

            // Incoming files come first, whatever page is open.
            if root.offer-from != "": Card {
                highlight: true;
                tint: Theme.catch;
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
                        PillButton { text: "Always catch"; label: "Always accept files from " + root.offer-from; clicked => { root.answer(2); } }
                        PillButton { text: "Catch"; label: "Accept the files"; icon: Icons.down; primary: true; warm: true; clicked => { root.answer(1); } }
                    }
                }
            }

            // What's happening right now.
            if (root.page == 0 || root.page == 2) && root.offer-from == "" && root.act-kind != 0: Card {
                VerticalLayout {
                    padding: 16px;
                    spacing: 12px;
                    HorizontalLayout {
                        spacing: 14px;
                        VerticalLayout {
                            alignment: center;
                            if root.act-kind == 6: CaughtBadge { }
                            if root.act-kind != 6: Badge { icon: root.kind-icon; tint: root.kind-color; }
                        }
                        VerticalLayout {
                            alignment: center;
                            spacing: 3px;
                            horizontal-stretch: 1;
                            Text { text: root.act-title; font-size: 16px; font-weight: 700; color: Theme.text; overflow: elide; }
                            Text { text: root.act-detail; font-size: 12px; color: Theme.muted; wrap: word-wrap; }
                        }
                    }
                    if root.act-kind == 1 || root.act-kind == 2: Flight {
                        progress: max(root.progress, 0);
                        ink: root.kind-color;
                        incoming: root.act-kind == 2;
                        peer-initial: root.act-peer-initial;
                        peer-tint: root.act-peer-tint;
                    }
                    if root.act-folder: HorizontalLayout {
                        spacing: 8px;
                        alignment: end;
                        if root.act-open: PillButton { text: "Open"; label: "Open the received file"; primary: true; warm: true; clicked => { root.open-saved(); } }
                        PillButton { text: "Show in folder"; icon: Icons.folder; clicked => { root.show-saved(); } }
                    }
                    if root.progress >= 0: Bar { value: root.progress; fill: root.kind-color; }
                }
            }

            // Items dropped on the window wait here for a device's Toss button.
            if root.page == 0 && root.dropped > 0: Card {
                highlight: true;
                HorizontalLayout {
                    padding: 14px;
                    spacing: 12px;
                    VerticalLayout { alignment: center; Badge { icon: Icons.up; } }
                    VerticalLayout {
                        alignment: center;
                        spacing: 2px;
                        horizontal-stretch: 1;
                        Text { text: root.dropped == 1 ? "1 item ready" : root.dropped + " items ready"; font-size: 15px; font-weight: 700; color: Theme.text; }
                        Text { text: root.devices.length > 0 ? "Pick a device below" : "Add a device first"; font-size: 12px; color: Theme.muted; }
                    }
                    VerticalLayout {
                        alignment: center;
                        PillButton { text: "Cancel"; label: "Forget the dropped items"; clicked => { root.drop-cancel(); } }
                    }
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
                        // 375x150 pixels: drawn 1:1 at 125% scaling, since the renderer resizes roughly
                        Image { source: @image-url("assets/empty.png"); width: 300px; height: 120px; accessible-role: none; }
                    }
                    Text { text: "No devices"; font-family: Theme.display-font; font-size: 19px; font-weight: 700; color: Theme.text; horizontal-alignment: center; }
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
                            enabled: invite-name.text != "";
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

            if root.page == 2 && root.phone-url != "": Card {
                VerticalLayout {
                    padding: 16px;
                    spacing: 8px;
                    HorizontalLayout {
                        alignment: center;
                        Rectangle {
                            width: 212px;
                            height: 212px;
                            border-radius: 12px;
                            background: white;
                            Image {
                                x: 6px;
                                y: 6px;
                                width: 200px;
                                height: 200px;
                                source: root.phone-qr;
                                image-rendering: pixelated;
                                accessible-role: none;
                            }
                        }
                    }
                    Text { text: "Scan with the phone camera"; font-size: 15px; font-weight: 700; color: Theme.text; horizontal-alignment: center; }
                    Text {
                        text: "Home network only: the page isn't encrypted.\n" + root.phone-url;
                        font-size: 12px;
                        color: Theme.muted;
                        horizontal-alignment: center;
                        wrap: word-wrap;
                    }
                    if root.phone-peer != "": Text {
                        text: root.phone-peer;
                        font-size: 12px;
                        color: Theme.accent;
                        horizontal-alignment: center;
                        wrap: word-wrap;
                    }
                }
            }
            if root.page == 2 && root.phone-url != "": Card {
                HorizontalLayout {
                    padding: 16px;
                    spacing: 12px;
                    VerticalLayout { alignment: center; Badge { icon: Icons.up; } }
                    VerticalLayout {
                        alignment: center;
                        spacing: 2px;
                        horizontal-stretch: 1;
                        Text { text: "Offered to the phone"; font-size: 15px; font-weight: 700; color: Theme.text; }
                        Text { text: root.phone-offered != "" ? root.phone-offered : "None"; font-size: 12px; color: Theme.muted; wrap: word-wrap; }
                    }
                    VerticalLayout {
                        alignment: center;
                        PillButton { text: "Choose"; label: "Choose files for the phone"; clicked => { root.phone-offer(); } }
                    }
                }
            }
            if root.page == 2 && root.phone-error != "": Text { text: root.phone-error; color: Theme.bad; font-size: 13px; wrap: word-wrap; }
            if root.page != 0 || root.devices.length == 0: Rectangle { vertical-stretch: 1; }
        }

        // Files dragged over the window.
        if root.drop-hover: Rectangle {
            x: 10px;
            y: 10px;
            width: root.width - 20px;
            height: root.height - 20px;
            border-radius: 18px;
            border-width: 2px;
            border-color: Theme.accent;
            background: Theme.bg-bottom.transparentize(0.08);
            VerticalLayout {
                alignment: center;
                spacing: 12px;
                HorizontalLayout { alignment: center; FileGlyph { width: 28px; height: 36px; fill: Theme.accent; } }
                Text {
                    text: root.page == 2 ? "Drop to offer to the phone" : "Drop to toss";
                    font-family: Theme.display-font;
                    font-size: 24px;
                    font-weight: 700;
                    color: Theme.text;
                    horizontal-alignment: center;
                }
            }
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
const CAUGHT: i32 = 6;

struct Gui {
    win: slint::Weak<AppWindow>,
    home: PathBuf,
    pending: Mutex<Option<oneshot::Sender<Answer>>>, // the Accept/Decline card waiting for a click
    transfer: Mutex<String>,                         // headline while bytes move, e.g. "Sending to laptop"
    phone: Mutex<Option<crate::phone::Share>>,       // the phone page's server, while that page is open
    saved: Mutex<Vec<PathBuf>>,                      // what the last "Received" card's buttons open
    dropped: Mutex<Vec<PathBuf>>,                    // dragged onto the window, waiting for a device (or offered to the phone)
    inviting: Mutex<Option<tokio::task::AbortHandle>>, // the invite link that's waiting, so a new one can replace it
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

    /// One file or folder dropped on the window. On the phone page files go straight to the phone;
    /// elsewhere they wait for a device's Toss button.
    fn drop_in(&self, w: &AppWindow, path: PathBuf) {
        let mut dropped = self.dropped.lock().expect("not poisoned");
        if w.get_page() != 2 {
            dropped.push(path);
            w.set_dropped(dropped.len() as i32);
            return;
        }
        if !path.is_file() {
            return; // the phone page offers files, not folders
        }
        dropped.push(path);
        if let Some(share) = &*self.phone.lock().expect("not poisoned") {
            share.offer(dropped.clone());
        }
        let names = dropped.iter().map(|f| f.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect();
        w.set_phone_offered(listed(names).into());
    }

    /// The dropped items, if any are waiting; taking them clears the banner.
    fn take_dropped(&self) -> Option<Vec<PathBuf>> {
        let dropped = std::mem::take(&mut *self.dropped.lock().expect("not poisoned"));
        self.update(|w| w.set_dropped(0));
        (!dropped.is_empty()).then_some(dropped)
    }

    /// Who's at the other end of the flight arc.
    fn peer(&self, name: &str) {
        let (letter, color) = (initial(name), tint(name));
        self.update(move |w| {
            w.set_act_peer_initial(letter);
            w.set_act_peer_tint(color);
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

/// Up to four names, one per line, then "and 3 more".
fn listed(names: Vec<String>) -> String {
    let mut text = names[..names.len().min(4)].join("\n");
    if names.len() > 4 {
        text += &format!("\nand {} more", names.len() - 4);
    }
    text
}

/// The phone page's link as a QR code image, 5 pixels per module.
fn qr_image(url: &str) -> SharedPixelBuffer<Rgb8Pixel> {
    const SCALE: usize = 5;
    let rows = crate::phone::qr(url);
    let side = rows.len() * SCALE;
    let mut image = SharedPixelBuffer::<Rgb8Pixel>::new(side as u32, side as u32);
    for (i, pixel) in image.make_mut_slice().iter_mut().enumerate() {
        let v = if rows[i / side / SCALE][i % side / SCALE] { 0 } else { 255 };
        *pixel = Rgb8Pixel { r: v, g: v, b: v };
    }
    image
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
                *self.transfer.lock().expect("not poisoned") = format!("Tossing to {to}");
                self.peer(to);
                self.show(SENDING, format!("Tossing to {to}"), format!("Waiting for approval · {}", count(files)), false);
            }
            Event::Sending { to, files } => {
                *self.transfer.lock().expect("not poisoned") = format!("Tossing to {to}");
                self.peer(to);
                self.show(SENDING, format!("Tossing to {to}"), count(files), false);
            }
            Event::Sent { to, files, direct, mb_per_s, resumed } => {
                let route = if direct { "direct" } else { "relayed" };
                let speed = mb_per_s.map(|s| format!(" · {s:.1} MB/s")).unwrap_or_default();
                let detail = format!("{} · {route}{speed}{}", count(files), resumed_note(resumed));
                self.show(DONE, format!("Tossed to {to}"), detail, false);
            }
            Event::Receiving { from, files } => {
                *self.transfer.lock().expect("not poisoned") = format!("Catching from {from}");
                self.peer(from);
                self.show(RECEIVING, format!("Catching from {from}"), count(files), false);
            }
            Event::Checking { bytes } => {
                let detail = format!("Checking {} from an earlier try", human(bytes));
                self.update(move |w| w.set_act_detail(detail.into()));
            }
            Event::Received { from, files, resumed, saved } => {
                // shortcut: no Open button on macOS and Linux, where received files carry no "downloaded" mark,
                // so Open could launch a .terminal or .desktop file unchecked; add macOS's quarantine flag,
                // tested on a Mac, to offer it there.
                let one_file = cfg!(windows) && matches!(saved, [file] if file.is_file());
                // Name a single file, so a program posing as something else is easier to spot.
                let what = match saved {
                    [item] if files == 1 => item.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                    _ => count(files),
                };
                *self.saved.lock().expect("not poisoned") = saved.to_vec();
                self.show(CAUGHT, format!("Caught from {from}"), format!("{what} · Downloads{}", resumed_note(resumed)), true);
                self.update(move |w| w.set_act_open(one_file));
            }
            Event::Declined { from } => self.show(NOTE, "Transfer declined".into(), format!("From {from}"), false),
            Event::PhoneConnected(ip) => {
                let peer = format!("Connected: {ip} · other devices are turned away");
                self.update(move |w| w.set_phone_peer(peer.into()));
            }
            Event::Failed(why) => self.show(PROBLEM, "Transfer failed".into(), why, false),
        }
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

    fn accept(&self, from: &str, files: &[(String, u64)], gone: impl Future<Output = ()> + Send) -> impl Future<Output = Answer> + Send {
        let total: u64 = files.iter().map(|f| f.1).sum();
        let summary = format!("{} · {}", count(files.len()), human(total));
        let names = listed(files.iter().map(|f| f.0.clone()).collect());
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
            let answer = tokio::select! {
                answer = rx => answer.unwrap_or(Answer::No),
                () = gone => Answer::No, // they gave up or sent again: take the question down
            };
            self.update(|w| w.set_offer_from("".into()));
            if !matches!(answer, Answer::No) {
                *self.transfer.lock().expect("not poisoned") = format!("Catching from {from}");
                self.peer(&from);
                self.show(RECEIVING, format!("Catching from {from}"), summary, false);
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
    let gui = Arc::new(Gui {
        win: win.as_weak(),
        home,
        pending: Mutex::new(None),
        transfer: Mutex::default(),
        phone: Mutex::default(),
        saved: Mutex::default(),
        inviting: Mutex::default(),
        dropped: Mutex::default(),
    });
    gui.refresh_devices();
    // Pairing or removing devices from the terminal changes contacts.json behind the window's back.
    let watch = slint::Timer::default();
    let (g, mut seen) = (gui.clone(), None);
    watch.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(2), move || {
        let modified = std::fs::metadata(g.home.join("contacts.json")).and_then(|m| m.modified()).ok();
        if modified != seen {
            seen = modified;
            g.refresh_devices();
        }
    });

    let g = gui.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::listen(g.clone()).await {
            g.update(|w| w.set_receiving(false));
            g.show(PROBLEM, "Not receiving".into(), format!("{e:#}"), false);
        }
    });

    let g = gui.clone();
    win.on_send_files(sender(&gui, move || g.take_dropped().or_else(|| rfd::FileDialog::new().pick_files())));
    win.on_send_folder(sender(&gui, || rfd::FileDialog::new().pick_folder().map(|folder| vec![folder])));
    let g = gui.clone();
    win.on_drop_cancel(move || drop(g.take_dropped()));
    // Files dragged in from Explorer or Finder: winit reports each one hovering, then dropped.
    // shortcut: Slint has no file-drop API yet, so this uses its winit hook (marked unstable), and
    // winit 0.30 can't do drag and drop on Wayland; move over when Slint gets its own.
    let (g, w) = (gui.clone(), win.as_weak());
    win.window().on_winit_window_event(move |_, event| {
        let Some(w) = w.upgrade() else { return EventResult::Propagate };
        match event {
            WindowEvent::HoveredFile(_) if !w.get_drop_hover() => {
                g.dropped.lock().expect("not poisoned").clear(); // a new drag replaces what was waiting
                w.set_drop_hover(true);
            }
            WindowEvent::HoveredFileCancelled => w.set_drop_hover(false),
            WindowEvent::DroppedFile(path) => {
                w.set_drop_hover(false);
                g.drop_in(&w, path.clone());
            }
            _ => {}
        }
        EventResult::Propagate
    });
    let g = gui.clone();
    win.on_open_saved(move || {
        let saved = g.saved.lock().expect("not poisoned").clone();
        if let Some(Err(e)) = saved.first().map(|file| open_file(file)) {
            g.show(PROBLEM, "Couldn't open it".into(), e.to_string(), false);
        }
    });
    let g = gui.clone();
    win.on_show_saved(move || {
        let saved = g.saved.lock().expect("not poisoned").clone();
        if let Some(Err(e)) = saved.first().map(|item| reveal(item)) {
            g.show(PROBLEM, "Couldn't show it".into(), e.to_string(), false);
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
        g.update(|w| {
            w.set_pair_error("".into());
            w.set_link("".into());
        });
        let task = tokio::spawn({
            let g = g.clone();
            async move {
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
            }
        });
        // A new link replaces one nobody has used, say after a typo in the name.
        if let Some(old) = g.inviting.lock().expect("not poisoned").replace(task.abort_handle()) {
            old.abort();
        }
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

    let g = gui.clone();
    win.on_phone_open(move || {
        let started = crate::phone::start(g.clone());
        let shown = started.as_ref().map(|share| (share.url.clone(), qr_image(&share.url))).map_err(|e| format!("{e:#}"));
        *g.phone.lock().expect("not poisoned") = started.ok();
        drop(g.take_dropped()); // a fresh phone page starts with nothing offered
        g.update(move |w| {
            w.set_phone_offered("".into());
            w.set_phone_peer("".into());
            match shown {
                Ok((url, qr)) => {
                    w.set_phone_error("".into());
                    w.set_phone_qr(Image::from_rgb8(qr));
                    w.set_phone_url(url.into());
                }
                Err(e) => {
                    w.set_phone_url("".into());
                    w.set_phone_error(e.into());
                }
            }
        });
    });
    let g = gui.clone();
    win.on_phone_offer(move || {
        let Some(files) = rfd::FileDialog::new().pick_files() else { return };
        let names = listed(files.iter().map(|f| f.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect());
        if let Some(share) = &*g.phone.lock().expect("not poisoned") {
            share.offer(files);
        }
        g.update(move |w| w.set_phone_offered(names.into()));
    });
    let g = gui.clone();
    win.on_phone_close(move || {
        drop(g.phone.lock().expect("not poisoned").take());
        drop(g.take_dropped()); // files dropped for the phone mustn't wait for a Toss button
    });
    let g = gui.clone();
    win.window().on_close_requested(move || {
        drop(g.phone.lock().expect("not poisoned").take()); // the phone link only works while its page shows
        drop(g.take_dropped());
        g.update(|w| {
            if w.get_page() == 2 {
                w.set_page(0);
            }
        });
        slint::CloseRequestResponse::HideWindow
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

/// A Toss button handler: get what to send (dropped items, or a picker), then send it in the background.
fn sender(gui: &Arc<Gui>, pick: impl Fn() -> Option<Vec<PathBuf>> + 'static) -> impl Fn(SharedString) + 'static {
    let gui = gui.clone();
    move |name| {
        let Some(paths) = pick() else { return };
        let gui = gui.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::send(&name, &paths, &*gui).await {
                gui.show(PROBLEM, format!("Tossing to {name} failed"), format!("{e:#}"), false);
            }
        });
    }
}

/// Opens a file in its default app: a picture in the photo viewer, a spreadsheet in Excel, and so on.
fn open_file(file: &Path) -> std::io::Result<()> {
    let mut cmd = Command::new(opener());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::raw_arg(&mut cmd, format!("\"{}\"", file.display())); // unquoted, explorer cuts the path at a comma
    #[cfg(not(windows))]
    cmd.arg(file);
    cmd.spawn().map(drop)
}

/// Shows a file or folder selected in its folder.
fn reveal(item: &Path) -> std::io::Result<()> {
    let mut cmd = Command::new(opener());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::raw_arg(&mut cmd, format!("/select,\"{}\"", item.display())); // explorer's own argument rules
    #[cfg(target_os = "macos")]
    cmd.arg("-R").arg(item);
    #[cfg(all(unix, not(target_os = "macos")))]
    cmd.arg(item.parent().unwrap_or(item)); // no standard way to select a file: open its folder
    cmd.spawn().map(drop)
}

fn opener() -> &'static str {
    if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Started from Explorer, Windows gives a console program a console window; close it.
#[cfg(windows)]
fn hide_console() {
    unsafe extern "system" {
        safe fn FreeConsole() -> i32;
        safe fn SetStdHandle(which: u32, handle: *mut std::ffi::c_void) -> i32;
    }
    // shortcut: the console still flashes for a moment; ship a separate windows-subsystem binary with the installers.
    FreeConsole();
    // The old console handles are dead now, and Windows refuses to start a program (Explorer, Photos)
    // that would inherit them: forget them.
    for which in [-10i32, -11, -12] {
        SetStdHandle(which as u32, std::ptr::null_mut()); // stdin, stdout, stderr
    }
}

#[cfg(not(windows))]
fn hide_console() {}
