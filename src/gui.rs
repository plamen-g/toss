//! The window: your devices, what's happening right now, an Accept/Decline card for incoming files,
//! an "Add device" page for pairing, and a "Phone" page with a QR code for a phone's browser. It keeps
//! receiving from the tray after the window closes.

use std::{
    future::Future,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use slint::{Color, ComponentHandle, Image, ModelRc, Rgb8Pixel, SharedPixelBuffer, SharedString, VecModel};
use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
use tokio::{io::AsyncReadExt, sync::oneshot};

use crate::{Answer, Event, Ui, count, human};

slint::slint! {
    import { CheckBox, ListView, Palette } from "std-widgets.slint";
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

    // Where files can be dropped: a dotted outline, the usual sign for it, drawn with the flight arc's
    // dots. Clicking it opens the picker, since people click drop zones too.
    component DropZone inherits Rectangle {
        in property <string> text;
        callback clicked;
        private property <color> dot: touch.has-hover || focus.has-focus ? Theme.accent : Theme.muted.transparentize(0.45);
        accessible-role: button;
        accessible-label: root.text;
        accessible-action-default => { root.clicked(); }
        height: 60px;
        vertical-stretch: 0;

        for i in 28: Rectangle { // top and bottom edges
            x: 14px + (root.width - 28px) * (mod(i, 14) / 13.0) - 1.5px;
            y: i < 14 ? 0px : root.height - 3px;
            width: 3px;
            height: 3px;
            border-radius: 1.5px;
            background: root.dot;
        }
        for i in 6: Rectangle { // left and right edges
            x: i < 3 ? 0px : root.width - 3px;
            y: 14px + (root.height - 28px) * (mod(i, 3) / 2.0) - 1.5px;
            width: 3px;
            height: 3px;
            border-radius: 1.5px;
            background: root.dot;
        }
        HorizontalLayout {
            alignment: center;
            spacing: 10px;
            VerticalLayout { alignment: center; FileGlyph { fill: root.dot; } }
            Text { text: root.text; font-size: 13px; color: Theme.muted; vertical-alignment: center; }
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
        callback choose-files(); // the drop zone was clicked
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

            if root.page == 0 && root.devices.length > 0 && root.dropped == 0: DropZone {
                text: "Drop files here, or click to choose";
                clicked => { root.choose-files(); }
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
            // As on the main page: a drop zone until something is offered, then what's offered.
            if root.page == 2 && root.phone-url != "" && root.phone-offered == "": DropZone {
                text: "Drop files for the phone, or click to choose";
                clicked => { root.phone-offer(); }
            }
            if root.page == 2 && root.phone-url != "" && root.phone-offered != "": Card {
                HorizontalLayout {
                    padding: 16px;
                    spacing: 12px;
                    VerticalLayout { alignment: center; Badge { icon: Icons.up; } }
                    VerticalLayout {
                        alignment: center;
                        spacing: 2px;
                        horizontal-stretch: 1;
                        Text { text: "Offered to the phone"; font-size: 15px; font-weight: 700; color: Theme.text; }
                        Text { text: root.phone-offered; font-size: 12px; color: Theme.muted; wrap: word-wrap; }
                    }
                    VerticalLayout {
                        alignment: center;
                        PillButton { text: "Choose"; label: "Choose other files for the phone"; clicked => { root.phone-offer(); } }
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
        in property <bool> autostart;
        in property <bool> explorer; // toss is in Explorer's "Send to" menu
        in property <bool> windows;
        in property <bool> claude-found; // Claude Code is installed
        in property <bool> in-claude; // and it runs toss
        in property <[string]> folders; // assistants send from these without asking: the user said "don't ask again"
        callback open();
        callback toggle-autostart();
        callback toggle-explorer();
        callback toggle-claude();
        callback forget(string);
        callback quit();
        clicked => { root.open(); }
        // Slint's tray menus don't draw check marks (checked), so the title carries one.
        Menu {
            MenuItem { title: "Open toss"; activated => { root.open(); } }
            MenuItem { title: (root.autostart ? "✓ " : "") + "Start at login"; activated => { root.toggle-autostart(); } }
            if root.windows: MenuItem { title: (root.explorer ? "✓ " : "") + "In Explorer's Send to menu"; activated => { root.toggle-explorer(); } }
            if root.claude-found: MenuItem { title: (root.in-claude ? "✓ " : "") + "Use with Claude Code"; activated => { root.toggle-claude(); } }
            if root.folders.length > 0: Menu {
                title: "Assistants send without asking from";
                for folder in root.folders: MenuItem { title: "✓ " + folder; activated => { root.forget(folder); } }
            }
            MenuItem { title: "Quit"; activated => { root.quit(); } }
        }
    }

    // toss's own question before an assistant's send (`toss ask`, for `toss mcp`): a window of its own,
    // so no assistant or app can answer it for the user.
    export component AskWindow inherits Window {
        title: "toss";
        icon: @image-url("assets/icon.png");
        always-on-top: true;
        preferred-width: 520px;
        default-font-size: 14px;
        background: @linear-gradient(180deg, Theme.bg-top 0%, Theme.bg-bottom 100%);
        init => { Palette.color-scheme = ColorScheme.dark; }

        in property <string> question;
        in property <string> details;
        in property <string> yes;
        in property <string> remember-label; // "" when there's no folder to remember
        in-out property <bool> remember;
        callback answer(bool);

        VerticalLayout {
            padding: 22px;
            spacing: 14px;
            Text { text: root.question; color: Theme.text; font-size: 16px; font-weight: 600; wrap: word-wrap; }
            Text { text: root.details; color: Theme.muted; wrap: char-wrap; } // paths have no spaces to break at
            if root.remember-label != "": CheckBox { text: root.remember-label; checked <=> root.remember; }
            HorizontalLayout {
                alignment: end;
                spacing: 10px;
                PillButton { text: "Don't send"; clicked => { root.answer(false); } }
                PillButton { text: root.yes; primary: true; clicked => { root.answer(true); } }
            }
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

    /// Items picked all at once: they replace what was waiting. (Not take_dropped(): its count reset
    /// runs later and would hide the banner these set.)
    fn drop_all(&self, w: &AppWindow, items: Vec<PathBuf>) {
        self.dropped.lock().expect("not poisoned").clear();
        for item in items {
            self.drop_in(w, item); // the same as dropping them: they wait for a device's Toss button
        }
    }

    /// Brings the window up, from the tray or from behind other windows, with `items` (if any) waiting.
    fn raise(&self, w: &AppWindow, items: Vec<PathBuf>) {
        if !items.is_empty() {
            self.drop_all(w, items);
        }
        let _ = w.show();
        w.window().set_minimized(false);
        w.window().with_winit_window(|w| w.focus_window());
    }

    /// Brings the window up on its phone page, offering `files`.
    fn phone_page(self: &Arc<Self>, w: &AppWindow, files: Vec<PathBuf>) {
        self.raise(w, vec![]);
        w.set_page(2);
        w.invoke_phone_open();
        // After phone-open's own update, which empties the list of what's offered.
        let g = self.clone();
        self.update(move |w| g.drop_all(&w, files));
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

/// Opens the window with `items` waiting for a device (Explorer's "Send to"), or with
/// `hidden`, only the tray icon (at login).
pub fn run(items: Vec<PathBuf>, hidden: bool) -> Result<()> {
    hide_console();
    // Just downloaded and opened: install, rather than run from Downloads.
    #[cfg(windows)]
    if items.is_empty() && !hidden && downloaded(&std::env::current_exe()?) {
        match install() {
            Ok(()) => return Ok(()),
            Err(e) => alert(&format!("Couldn't install toss, so it runs from where it is: {e:#}")),
        }
    }
    open(items, hidden, false)
}

/// The window on its phone page, offering `files` (for an assistant, through `toss mcp`).
pub fn run_phone(files: Vec<PathBuf>) -> Result<()> {
    hide_console();
    open(files, false, true)
}

/// The first start of a toss that `install` just put in place: it goes in the Start menu, starts at
/// login and joins Claude Code (when that's installed), then opens.
pub fn run_installed() -> Result<()> {
    hide_console();
    let _ = std::fs::remove_file(std::env::current_exe()?.with_file_name("toss.old")); // what an update replaced, unless it still runs
    let setup = (|| -> Result<()> {
        #[cfg(windows)]
        shortcut(&start_menu()?, "")?;
        set_autostart(true)?;
        if find_claude().is_some() {
            set_claude_code(true)?;
        }
        Ok(())
    })();
    if let Err(e) = setup {
        alert(&format!("toss is installed, but: {e:#}"));
    }
    open(vec![], false, false)
}

/// `items` wait for a device, or with `phone`, are offered on the phone page.
fn open(items: Vec<PathBuf>, hidden: bool, phone: bool) -> Result<()> {
    let home = crate::home()?;
    // shortcut: the probe lock is released before listen() takes it, so two copies started at the
    // same instant could both pass; harmless, the second's listen() then reports the clash.
    if crate::lock_listener(&home).is_err() {
        // The toss that's running shows its window, with the items.
        if !hidden && hand_over(&home, Some(&items), phone).is_err() {
            alert("toss is already running. Open it from its icon in the system tray (near the clock).");
        }
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
    // For the phone page, the items wait until that page is set up, below.
    let for_phone = if phone {
        Some(items)
    } else {
        gui.drop_all(&win, items);
        None
    };

    // Another toss started while this one runs hands over its items through this local port, with a
    // secret only this user can read.
    let others = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let secret = crate::hex(&iroh::SecretKey::generate().to_bytes()[..16]);
    crate::write_private(&gui.home.join("window"), format!("{} {secret}", others.local_addr()?.port()).as_bytes())?;
    others.set_nonblocking(true)?;
    let others = tokio::net::TcpListener::from_std(others)?;
    let g = gui.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = others.accept().await {
            let mut bytes = Vec::new();
            let read = tokio::time::timeout(std::time::Duration::from_secs(5), (&mut stream).take(1 << 20).read_to_end(&mut bytes)).await;
            if !matches!(read, Ok(Ok(_))) {
                continue;
            }
            let parsed = serde_json::from_slice::<(String, Option<Vec<PathBuf>>)>(&bytes)
                .map(|(got, items)| (got, items, false))
                .or_else(|_| serde_json::from_slice::<(String, Option<Vec<PathBuf>>, bool)>(&bytes));
            let Ok((got, items, phone)) = parsed else { continue };
            if !crate::same(got.as_bytes(), secret.as_bytes()) {
                continue;
            }
            match items {
                Some(items) if phone => {
                    let g2 = g.clone();
                    g.update(move |w| g2.phone_page(&w, items));
                }
                Some(items) => {
                    let g2 = g.clone();
                    g.update(move |w| g2.raise(&w, items));
                }
                // An update asks this toss to quit, so the new one can take its place.
                None => drop(slint::invoke_from_event_loop(|| drop(slint::quit_event_loop()))),
            }
        }
    });

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
    let (g, w) = (gui.clone(), win.as_weak());
    win.on_choose_files(move || {
        let (Some(w), Some(files)) = (w.upgrade(), rfd::FileDialog::new().pick_files()) else { return };
        g.drop_all(&w, files);
    });
    // Files dragged in from Explorer or Finder: winit reports each one hovering, then dropped.
    // shortcut: Slint has no file-drop API yet, so this uses its winit hook (marked unstable), and
    // winit 0.30 can't do drag and drop on Wayland; move over when Slint gets its own.
    let (g, w) = (gui.clone(), win.as_weak());
    win.window().on_winit_window_event(move |_, event| {
        let Some(w) = w.upgrade() else { return EventResult::Propagate };
        match event {
            WindowEvent::HoveredFile(_) => w.set_drop_hover(true),
            WindowEvent::HoveredFileCancelled => w.set_drop_hover(false),
            WindowEvent::DroppedFile(path) => {
                if w.get_drop_hover() {
                    g.dropped.lock().expect("not poisoned").clear(); // a new drop replaces what was waiting; a drag that leaves doesn't
                    w.set_drop_hover(false);
                }
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
    // Keep the login item and the Explorer entry pointing at this copy of toss, in case it moved.
    if autostart() {
        let _ = set_autostart(true);
    }
    if explorer_menu() {
        let _ = set_explorer_menu(true);
    }
    tray.set_windows(cfg!(windows));
    tray.set_autostart(autostart());
    tray.set_explorer(explorer_menu());
    let t = tray.as_weak();
    tray.on_toggle_autostart(move || {
        let Some(t) = t.upgrade() else { return };
        if let Err(e) = set_autostart(!t.get_autostart()) {
            alert(&format!("Couldn't change starting at login: {e:#}"));
        }
        t.set_autostart(autostart());
    });
    let t = tray.as_weak();
    tray.on_toggle_explorer(move || {
        let Some(t) = t.upgrade() else { return };
        if let Err(e) = set_explorer_menu(!t.get_explorer()) {
            alert(&format!("Couldn't change Explorer's Send to menu: {e:#}"));
        }
        t.set_explorer(explorer_menu());
    });
    tray.set_claude_found(find_claude().is_some());
    tray.set_in_claude(in_claude().is_some());
    let t = tray.as_weak();
    tray.on_toggle_claude(move || {
        let Some(t) = t.upgrade() else { return };
        if let Err(e) = set_claude_code(!t.get_in_claude()) {
            alert(&format!("Couldn't change Claude Code's settings: {e:#}"));
        }
        t.set_in_claude(in_claude().is_some());
    });
    tray.set_folders(folder_list(&gui.home));
    let (t, home) = (tray.as_weak(), gui.home.clone());
    tray.on_forget(move |folder| {
        if let Err(e) = crate::mcp::forget(&home, &folder) {
            alert(&format!("Couldn't change the list: {e:#}"));
        }
        if let Some(t) = t.upgrade() {
            t.set_folders(folder_list(&home));
        }
    });
    // `toss mcp` runs apart, one for each assistant: keep up with the folders it was told not to ask
    // about again, and show what it sends.
    fn modified(file: &Path) -> Option<std::time::SystemTime> {
        std::fs::metadata(file).and_then(|m| m.modified()).ok()
    }
    let (folders, sent) = (gui.home.join("folders.json"), gui.home.join("sent.jsonl"));
    let mut seen = (modified(&folders), modified(&sent));
    let (g, t) = (gui.clone(), tray.as_weak());
    let mcp_watch = slint::Timer::default();
    mcp_watch.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(2), move || {
        let now = (modified(&folders), modified(&sent));
        if now.0 != seen.0
            && let Some(t) = t.upgrade()
        {
            t.set_folders(folder_list(&g.home));
        }
        if now.1 != seen.1
            && let Some(s) = last_sent(&sent)
        {
            let names = listed(s.files.iter().map(|f| f.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect());
            g.show(DONE, format!("{} sent {} to {}", s.by, count(s.files.len()), s.to), names, false);
        }
        seen = now;
    });
    tray.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // Closing the window only hides it (Slint's default); toss keeps receiving until Quit.
    if let Some(files) = for_phone {
        gui.phone_page(&win, files);
    } else if !hidden {
        win.show()?;
    }
    slint::run_event_loop_until_quit()?;
    let _ = std::fs::remove_file(gui.home.join("window"));
    Ok(())
}

/// Hands items to the toss that's already running, whose window then comes up with them waiting for a
/// device, or with `phone`, offering them on the phone page. None asks it to quit.
fn hand_over(home: &Path, items: Option<&[PathBuf]>, phone: bool) -> Result<()> {
    let text = std::fs::read_to_string(home.join("window"))?;
    let (port, secret) = text.split_once(' ').context("bad window file")?;
    let stream = std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>()?))?;
    // Without `phone`, the message is the one older tosses know.
    Ok(if phone { serde_json::to_writer(stream, &(secret, items, true)) } else { serde_json::to_writer(stream, &(secret, items)) }?)
}

/// Opens the phone page with `files` offered: in the toss that's running, else in a new one.
pub fn show_on_phone(home: &Path, files: &[PathBuf]) -> Result<()> {
    if crate::lock_listener(home).is_err() {
        return hand_over(home, Some(files), true).context("toss is running without its window");
    }
    let start = |out_of_job: bool| {
        let null = std::process::Stdio::null; // never `toss mcp`'s own input and output, which carry MCP
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.arg("--phone").args(files).stdin(null()).stdout(null()).stderr(null());
        // CREATE_NO_WINDOW, and CREATE_BREAKAWAY_FROM_JOB: an app that closes its MCP servers' processes
        // together would close this window with them.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000 | if out_of_job { 0x0100_0000 } else { 0 });
        cmd.spawn()
    };
    start(true).or_else(|_| start(false))?; // the app's job may not let it out
    Ok(())
}

/// The newest send `toss mcp` noted in `log`.
fn last_sent(log: &Path) -> Option<crate::AssistantSent> {
    serde_json::from_str(std::fs::read_to_string(log).ok()?.lines().last()?).ok()
}

/// The remembered folders, as the tray lists them.
fn folder_list(home: &Path) -> ModelRc<SharedString> {
    let folders: Vec<SharedString> = crate::mcp::remembered(home).iter().map(|f| crate::mcp::shown(f).into()).collect();
    ModelRc::new(VecModel::from(folders))
}

/// What `toss mcp` asks through `toss ask`: JSON on its input.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Question {
    pub question: String,
    pub details: String,
    pub yes: String,             // the button that sends
    pub remember: Option<String>, // the folder "don't ask again" would let through
}

/// The answer `toss ask` prints.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Said {
    pub send: bool,
    pub remember: bool,
}

/// `toss ask`: shows `toss mcp`'s question in a window of toss's own, then prints the answer.
pub fn ask() -> Result<()> {
    let question: Question = serde_json::from_reader(std::io::stdin())?;
    let win = AskWindow::new()?;
    win.set_question(question.question.into());
    win.set_details(question.details.into());
    win.set_yes(question.yes.into());
    win.set_remember_label(question.remember.map(|f| format!("Don't ask again for {f}")).unwrap_or_default().into());
    let send = std::rc::Rc::new(std::cell::Cell::new(false));
    let s = send.clone();
    win.on_answer(move |yes| {
        s.set(yes);
        let _ = slint::quit_event_loop();
    });
    win.show()?;
    slint::run_event_loop()?; // closing the window counts as no
    let said = Said { send: send.get(), remember: send.get() && win.get_remember() };
    println!("{}", serde_json::to_string(&said)?);
    Ok(())
}

fn alert(text: &str) {
    rfd::MessageDialog::new().set_title("toss").set_description(text).show();
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
                // The items wait again, so Toss retries (and resumes), unless others were dropped
                // meanwhile or the phone page, which offers what's dropped, is showing.
                let g = gui.clone();
                gui.update(move |w| {
                    let mut dropped = g.dropped.lock().expect("not poisoned");
                    if dropped.is_empty() && w.get_page() != 2 {
                        w.set_dropped(paths.len() as i32);
                        *dropped = paths;
                    }
                });
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

/// Where the system looks for programs to start at login: the Startup folder on Windows, LaunchAgents
/// on macOS, ~/.config/autostart on desktop Linux.
fn login_item() -> Result<PathBuf> {
    Ok(if cfg!(windows) {
        dirs::data_dir().context("no AppData folder")?.join(r"Microsoft\Windows\Start Menu\Programs\Startup\toss.lnk")
    } else if cfg!(target_os = "macos") {
        dirs::home_dir().context("no home folder")?.join("Library/LaunchAgents/com.github.plamen-g.toss.plist")
    } else {
        dirs::config_dir().context("no config folder")?.join("autostart/toss.desktop")
    })
}

/// Whether toss starts in the tray when you log in.
fn autostart() -> bool {
    login_item().is_ok_and(|item| item.exists())
}

fn set_autostart(on: bool) -> Result<()> {
    let item = login_item()?;
    if !on {
        return remove(&item);
    }
    std::fs::create_dir_all(item.parent().expect("inside a folder"))?;
    write_login_item(&item)
}

#[cfg(windows)]
fn write_login_item(item: &Path) -> Result<()> {
    shortcut(item, "--tray")
}

/// A LaunchAgent on macOS, a desktop entry on Linux.
#[cfg(not(windows))]
fn write_login_item(item: &Path) -> Result<()> {
    let exe = std::env::current_exe()?.to_str().context("toss's path isn't valid text")?.to_string();
    let text = if cfg!(target_os = "macos") {
        let exe = exe.replace('&', "&amp;").replace('<', "&lt;");
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.github.plamen-g.toss</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>--tray</string></array>
  <key>RunAtLoad</key><true/>
</dict></plist>
"#
        )
    } else {
        // Desktop files need these escaped in layers; refuse rather than write a broken entry.
        anyhow::ensure!(!exe.contains(['"', '`', '$', '\\', '%', '\n']), "toss's path has characters a login item can't hold");
        format!("[Desktop Entry]\nType=Application\nName=toss\nExec=\"{exe}\" --tray\n")
    };
    Ok(std::fs::write(item, text)?)
}

/// Explorer's "Send to" menu lists the shortcuts in this folder, and hands a shortcut's program every
/// selected item at once.
#[cfg(windows)]
fn send_to() -> Result<PathBuf> {
    Ok(dirs::data_dir().context("no AppData folder")?.join(r"Microsoft\Windows\SendTo\toss.lnk"))
}

/// Whether Explorer's "Send to" menu lists toss.
#[cfg(windows)]
fn explorer_menu() -> bool {
    send_to().is_ok_and(|item| item.exists())
}

#[cfg(windows)]
fn set_explorer_menu(on: bool) -> Result<()> {
    let item = send_to()?;
    if on { shortcut(&item, "") } else { remove(&item) }
}

#[cfg(not(windows))]
fn explorer_menu() -> bool {
    false // the tray menu hides the option
}

#[cfg(not(windows))]
fn set_explorer_menu(_: bool) -> Result<()> {
    Ok(())
}

fn remove(item: &Path) -> Result<()> {
    match std::fs::remove_file(item) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Whether this file came from the internet: Windows marks downloads with a Zone.Identifier stream.
#[cfg(windows)]
fn downloaded(file: &Path) -> bool {
    let mut mark = file.as_os_str().to_owned();
    mark.push(":Zone.Identifier");
    std::fs::read_to_string(mark).is_ok_and(|zone| zone.contains("ZoneId=3"))
}

/// Installs this download for the current user, into %LOCALAPPDATA%\Programs\toss, and starts it there.
/// A toss running already (an older version) is asked to quit first, so this one takes its place.
#[cfg(windows)]
fn install() -> Result<()> {
    let home = crate::home()?;
    if crate::lock_listener(&home).is_err() {
        let _ = hand_over(&home, None, false);
        for _ in 0..50 {
            if crate::lock_listener(&home).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    let dir = dirs::data_local_dir().context("no local AppData folder")?.join(r"Programs\toss");
    no_console(Command::new(install_copy(&std::env::current_exe()?, &dir)?).arg("--installed")).spawn()?;
    Ok(())
}

/// Puts a copy of `from` in `dir` as toss.exe. A toss.exe that still runs can't be overwritten, but it
/// can be moved aside, to toss.old.
#[cfg(windows)]
fn install_copy(from: &Path, dir: &Path) -> Result<PathBuf> {
    let bytes = std::fs::read(from)?;
    std::fs::create_dir_all(dir)?;
    let (exe, old) = (dir.join("toss.exe"), dir.join("toss.old"));
    let _ = std::fs::remove_file(&old);
    if exe.exists() {
        std::fs::rename(&exe, &old)?;
    }
    // Written afresh, as installers write: a copy would keep the download mark, and the installed toss
    // would then install itself again.
    std::fs::write(&exe, bytes)?;
    Ok(exe)
}

/// toss's entry in the Start menu, which installing adds.
#[cfg(windows)]
fn start_menu() -> Result<PathBuf> {
    Ok(dirs::data_dir().context("no AppData folder")?.join(r"Microsoft\Windows\Start Menu\Programs\toss.lnk"))
}

/// Starts a console program from the window without a console flashing up (on Windows).
fn no_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(cmd, 0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

/// Claude Code's command: on the PATH when it's installed for the terminal, otherwise the newest copy
/// the Claude app keeps for its Code tab.
fn find_claude() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) { &["claude.exe", "claude.cmd"] } else { &["claude"] };
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
        .or_else(|| newest_claude(&dirs::config_dir()?.join("Claude").join("claude-code")))
}

/// The newest of the Claude app's copies of Claude Code, kept as `root`/<version>/<build>/claude.
/// By date, not name: "2.1.100" sorts before "2.1.99".
fn newest_claude(root: &Path) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "claude.exe" } else { "claude" };
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .flat_map(|version| std::fs::read_dir(version.path()).into_iter().flatten().flatten())
        .map(|build| build.path().join(exe))
        .filter_map(|claude| Some((claude.metadata().ok()?.modified().ok()?, claude)))
        .max()
        .map(|(_, claude)| claude)
}

/// The toss Claude Code runs, if any: what `claude mcp add --scope user` saved in its config file.
fn in_claude() -> Option<PathBuf> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(dirs::home_dir)?;
    let config: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join(".claude.json")).ok()?).ok()?;
    config["mcpServers"]["toss"]["command"].as_str().map(PathBuf::from)
}

/// Adds this toss to Claude Code for every project, or takes it out, through Claude Code's own command,
/// which keeps its config file intact. No folders go without asking: toss asks before every send.
fn set_claude_code(on: bool) -> Result<()> {
    let claude = find_claude().context("Claude Code isn't installed")?;
    let run = |cmd: &mut Command| -> Result<()> {
        let out = no_console(cmd).output()?;
        anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
        Ok(())
    };
    // Out first, so adding it again points Claude Code at this copy of toss.
    let removed = run(Command::new(&claude).args(["mcp", "remove", "--scope", "user", "toss"]));
    if !on {
        return removed;
    }
    run(Command::new(&claude).args(["mcp", "add", "--scope", "user", "toss", "--"]).arg(std::env::current_exe()?).arg("mcp"))
}

/// Saves a shortcut that starts this toss with `args`.
#[cfg(windows)]
fn shortcut(path: &Path, args: &str) -> Result<()> {
    use windows::Win32::{
        System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile},
        UI::Shell::{IShellLinkW, ShellLink},
    };
    use windows::core::{HSTRING, Interface};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); // usually done already, for drag and drop
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(std::env::current_exe()?.as_os_str()))?;
        link.SetArguments(&HSTRING::from(args))?;
        link.cast::<IPersistFile>()?.Save(&HSTRING::from(path.as_os_str()), true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn a_download_installs_itself_without_its_download_mark() {
        let dir = std::env::temp_dir().join(format!("toss-install-test-{}", std::process::id()));
        let (download, programs) = (dir.join("toss-windows-x86_64.exe"), dir.join("Programs"));
        std::fs::create_dir_all(&programs).unwrap();
        std::fs::write(&download, b"new").unwrap();
        assert!(!super::downloaded(&download), "built here, not downloaded");
        std::fs::write(dir.join("toss-windows-x86_64.exe:Zone.Identifier"), "[ZoneTransfer]\r\nZoneId=3\r\n").unwrap();
        assert!(super::downloaded(&download), "the browser's mark");

        std::fs::write(programs.join("toss.exe"), b"old").unwrap();
        let installed = super::install_copy(&download, &programs).unwrap();
        assert_eq!(std::fs::read(&installed).unwrap(), b"new");
        assert_eq!(std::fs::read(programs.join("toss.old")).unwrap(), b"old", "the toss it replaces is moved aside");
        assert!(!super::downloaded(&installed), "else the installed toss would install itself again, and again");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finds_the_newest_claude_code_the_claude_app_keeps() {
        let root = std::env::temp_dir().join(format!("toss-claude-test-{}", std::process::id()));
        let exe = if cfg!(windows) { "claude.exe" } else { "claude" };
        let (older, newer) = (root.join("2.1.99").join("a1"), root.join("2.1.100").join("b2"));
        for dir in [&older, &newer] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(exe), b"").unwrap();
        }
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(older.join(exe)).unwrap().set_modified(hour_ago).unwrap();
        assert_eq!(super::newest_claude(&root), Some(newer.join(exe)));
        assert_eq!(super::newest_claude(&root.join("missing")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn saves_a_shortcut_with_its_arguments() {
        let path = std::env::temp_dir().join(format!("toss-test-{}.lnk", std::process::id()));
        super::shortcut(&path, "--tray").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(bytes[..4], [0x4c, 0, 0, 0], "a shortcut file");
        let tray: Vec<u8> = "--tray".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert!(bytes.windows(tray.len()).any(|w| w == tray), "it keeps the arguments");
    }
}
