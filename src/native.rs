use crate::AppController;
use gtk::gio;
use gtk::glib;
use gtk::glib::prelude::ToVariant;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/StatusNotifierMenu";
const ITEM_INTERFACE: &str = "org.kde.StatusNotifierItem";
const MENU_INTERFACE: &str = "com.canonical.dbusmenu";
const WATCHER_BUS: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";
const SETTINGS_MENU_ID: i32 = 1;
const QUIT_MENU_ID: i32 = 2;

pub struct TrayIndicator {
    connection: Option<gio::DBusConnection>,
    registration_id: Option<gio::RegistrationId>,
    menu_registration_id: Option<gio::RegistrationId>,
    bus_name: String,
    recording: Rc<Cell<bool>>,
}

impl TrayIndicator {
    pub fn new(app: Weak<RefCell<AppController>>) -> Self {
        let bus_name = format!(
            "org.kde.StatusNotifierItem.whisper_gtk_{}",
            std::process::id()
        );
        let recording = Rc::new(Cell::new(false));
        let mut indicator = Self {
            connection: None,
            registration_id: None,
            menu_registration_id: None,
            bus_name,
            recording: recording.clone(),
        };

        if let Err(err) = indicator.register(app, recording) {
            eprintln!("tray disabled: {err}");
        }

        indicator
    }

    pub fn set_recording(&self, recording: bool) {
        self.recording.set(recording);
        let Some(connection) = self.connection.as_ref() else {
            return;
        };

        let status = status_text(recording);
        let icon = icon_name(recording);
        let _ = connection.emit_signal(
            None,
            ITEM_PATH,
            ITEM_INTERFACE,
            "NewStatus",
            Some(&(status,).to_variant()),
        );
        let _ = connection.emit_signal(None, ITEM_PATH, ITEM_INTERFACE, "NewIcon", None);
        let _ = connection.emit_signal(
            None,
            ITEM_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            Some(
                &(
                    ITEM_INTERFACE,
                    std::collections::HashMap::from([
                        ("Status", status.to_variant()),
                        ("IconName", icon.to_variant()),
                    ]),
                    Vec::<String>::new(),
                )
                    .to_variant(),
            ),
        );
    }

    fn register(
        &mut self,
        app: Weak<RefCell<AppController>>,
        recording: Rc<Cell<bool>>,
    ) -> Result<(), glib::Error> {
        let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
        request_bus_name(&connection, &self.bus_name)?;

        let node = gio::DBusNodeInfo::for_xml(STATUS_NOTIFIER_XML)?;
        let interface = node
            .lookup_interface(ITEM_INTERFACE)
            .expect("StatusNotifierItem interface missing from introspection XML");
        let item_app = app.clone();
        let item_registration_id = connection
            .register_object(ITEM_PATH, &interface)
            .method_call(
                move |_connection, _sender, _path, _interface, method, _parameters, invocation| {
                    if matches!(method, "Activate" | "SecondaryActivate") {
                        if let Some(controller) = item_app.upgrade() {
                            controller.borrow_mut().show_settings_window();
                        }
                    }
                    invocation.return_value(Some(&().to_variant()));
                },
            )
            .property(move |_connection, _sender, _path, _interface, property| {
                status_notifier_property(property, recording.get())
            })
            .build()?;

        let menu_node = gio::DBusNodeInfo::for_xml(DBUS_MENU_XML)?;
        let menu_interface = menu_node
            .lookup_interface(MENU_INTERFACE)
            .expect("DBusMenu interface missing from introspection XML");
        let menu_app = app.clone();
        let menu_registration_id = connection
            .register_object(MENU_PATH, &menu_interface)
            .method_call(
                move |_connection, _sender, _path, _interface, method, parameters, invocation| {
                    match method {
                        "GetLayout" => {
                            invocation.return_value(Some(&menu_layout_response()));
                        }
                        "GetGroupProperties" => {
                            invocation.return_value(Some(&menu_group_properties_response()));
                        }
                        "GetProperty" => {
                            let property = parameters
                                .get::<(i32, String)>()
                                .map(|(id, property)| menu_property(id, &property))
                                .unwrap_or_else(|| ().to_variant());
                            invocation.return_value(Some(&(property,).to_variant()));
                        }
                        "AboutToShow" => {
                            invocation.return_value(Some(&(false,).to_variant()));
                        }
                        "AboutToShowGroup" => {
                            invocation.return_value(Some(
                                &(Vec::<i32>::new(), Vec::<i32>::new()).to_variant(),
                            ));
                        }
                        "EventGroup" => {
                            let errors = parameters
                                .get::<(Vec<(i32, String, glib::Variant, u32)>,)>()
                                .map(|(events,)| {
                                    events
                                        .into_iter()
                                        .filter_map(|(id, event_id, _data, _timestamp)| {
                                            (!handle_menu_event(&menu_app, id, &event_id))
                                                .then_some(id)
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            invocation.return_value(Some(&(errors,).to_variant()));
                        }
                        "Event" => {
                            if let Some((id, event_id, _data, _timestamp)) =
                                parameters.get::<(i32, String, glib::Variant, u32)>()
                            {
                                let _ = handle_menu_event(&menu_app, id, &event_id);
                            }
                            invocation.return_value(Some(&().to_variant()));
                        }
                        _ => {
                            invocation.return_value(Some(&().to_variant()));
                        }
                    }
                },
            )
            .property(|_connection, _sender, _path, _interface, property| {
                dbus_menu_property(property)
            })
            .build()?;

        connection.call_sync(
            Some(WATCHER_BUS),
            WATCHER_PATH,
            WATCHER_INTERFACE,
            "RegisterStatusNotifierItem",
            Some(&(self.bus_name.as_str(),).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2_000,
            gio::Cancellable::NONE,
        )?;

        self.connection = Some(connection);
        self.registration_id = Some(item_registration_id);
        self.menu_registration_id = Some(menu_registration_id);
        Ok(())
    }
}

impl Drop for TrayIndicator {
    fn drop(&mut self) {
        if let (Some(connection), Some(registration_id)) =
            (self.connection.as_ref(), self.registration_id.take())
        {
            let _ = connection.unregister_object(registration_id);
        }
        if let (Some(connection), Some(registration_id)) =
            (self.connection.as_ref(), self.menu_registration_id.take())
        {
            let _ = connection.unregister_object(registration_id);
        }
        if let Some(connection) = self.connection.as_ref() {
            let _ = connection.call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ReleaseName",
                Some(&(self.bus_name.as_str(),).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                1_000,
                gio::Cancellable::NONE,
            );
        }
    }
}

fn request_bus_name(connection: &gio::DBusConnection, bus_name: &str) -> Result<(), glib::Error> {
    connection.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "RequestName",
        Some(&(bus_name, 0u32).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        1_000,
        gio::Cancellable::NONE,
    )?;
    Ok(())
}

fn status_notifier_property(property: &str, recording: bool) -> glib::Variant {
    match property {
        "Category" => "ApplicationStatus".to_variant(),
        "Id" => "whisper-gtk".to_variant(),
        "Title" => "Whisper GTK".to_variant(),
        "Status" => status_text(recording).to_variant(),
        "IconName" => icon_name(recording).to_variant(),
        "AttentionIconName" => "media-record".to_variant(),
        "OverlayIconName" => "".to_variant(),
        "IconThemePath" => "".to_variant(),
        "Menu" => MENU_PATH.to_variant(),
        "ItemIsMenu" => false.to_variant(),
        "WindowId" => 0u32.to_variant(),
        "IconPixmap" | "AttentionIconPixmap" => icon_pixmap_variant(recording),
        "ToolTip" => glib::Variant::parse(
            Some(glib::VariantTy::new("(sa(iiay)ss)").unwrap()),
            r#"('', [], 'Whisper GTK', 'Click to open settings')"#,
        )
        .unwrap(),
        _ => ().to_variant(),
    }
}

fn dbus_menu_property(property: &str) -> glib::Variant {
    match property {
        "Version" => 3u32.to_variant(),
        "TextDirection" => "ltr".to_variant(),
        "Status" => "normal".to_variant(),
        "IconThemePath" => Vec::<String>::new().to_variant(),
        _ => ().to_variant(),
    }
}

fn menu_layout_response() -> glib::Variant {
    glib::Variant::parse(
        Some(glib::VariantTy::new("(u(ia{sv}av))").unwrap()),
        "(uint32 1, (0, {'children-display': <'submenu'>}, [\
            <(1, {'label': <'Settings'>, 'enabled': <true>, 'visible': <true>, 'type': <'standard'>}, @av [])>,\
            <(2, {'label': <'Quit'>, 'enabled': <true>, 'visible': <true>, 'type': <'standard'>}, @av [])>\
        ]))",
    )
    .unwrap()
}

fn menu_group_properties_response() -> glib::Variant {
    glib::Variant::parse(
        Some(glib::VariantTy::new("(a(ia{sv}))").unwrap()),
        "([(1, {'label': <'Settings'>, 'enabled': <true>, 'visible': <true>, 'type': <'standard'>}),\
           (2, {'label': <'Quit'>, 'enabled': <true>, 'visible': <true>, 'type': <'standard'>})],)",
    )
    .unwrap()
}

fn settings_menu_properties() -> HashMap<String, glib::Variant> {
    menu_item_properties("Settings")
}

fn quit_menu_properties() -> HashMap<String, glib::Variant> {
    menu_item_properties("Quit")
}

fn menu_item_properties(label: &str) -> HashMap<String, glib::Variant> {
    HashMap::from([
        ("label".to_string(), label.to_variant()),
        ("enabled".to_string(), true.to_variant()),
        ("visible".to_string(), true.to_variant()),
        ("type".to_string(), "standard".to_variant()),
    ])
}

fn menu_property(id: i32, property: &str) -> glib::Variant {
    let mut properties = match id {
        SETTINGS_MENU_ID => settings_menu_properties(),
        QUIT_MENU_ID => quit_menu_properties(),
        _ => return ().to_variant(),
    };
    properties
        .remove(property)
        .unwrap_or_else(|| ().to_variant())
}

fn handle_menu_event(app: &Weak<RefCell<AppController>>, id: i32, event_id: &str) -> bool {
    if event_id != "clicked" {
        return false;
    }

    let Some(controller) = app.upgrade() else {
        return true;
    };
    let mut controller = controller.borrow_mut();
    match id {
        SETTINGS_MENU_ID => controller.show_settings_window(),
        QUIT_MENU_ID => controller.quit(),
        _ => return false,
    }
    true
}

fn status_text(recording: bool) -> &'static str {
    if recording {
        "NeedsAttention"
    } else {
        "Active"
    }
}

fn icon_name(recording: bool) -> &'static str {
    if recording {
        "media-record"
    } else {
        "audio-input-microphone"
    }
}

fn icon_pixmap_variant(recording: bool) -> glib::Variant {
    vec![(22i32, 22i32, tray_icon_argb(recording))].to_variant()
}

fn tray_icon_argb(recording: bool) -> Vec<u8> {
    let background = if recording {
        [0xff, 0xd5, 0x3f, 0x3f]
    } else {
        [0xff, 0x2f, 0x6f, 0xed]
    };
    let foreground = [0xff, 0xff, 0xff, 0xff];
    let transparent = [0x00, 0x00, 0x00, 0x00];
    let mut pixels = Vec::with_capacity(22 * 22 * 4);

    for y in 0..22 {
        for x in 0..22 {
            let dx = x - 11;
            let dy = y - 11;
            let in_badge = dx * dx + dy * dy <= 100;
            let mic_head = (8..=13).contains(&x) && (5..=13).contains(&y);
            let mic_body = (9..=12).contains(&x) && (10..=14).contains(&y);
            let mic_stem = (10..=11).contains(&x) && (16..=18).contains(&y);
            let mic_base = (7..=14).contains(&x) && (18..=19).contains(&y);
            let mic_arc =
                (6..=16).contains(&x) && (12..=16).contains(&y) && (x == 6 || x == 16 || y == 16);

            let color = if mic_head || mic_body || mic_stem || mic_base || mic_arc {
                foreground
            } else if in_badge {
                background
            } else {
                transparent
            };
            pixels.extend_from_slice(&color);
        }
    }

    pixels
}

const STATUS_NOTIFIER_XML: &str = r#"
<node>
  <interface name="org.kde.StatusNotifierItem">
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="WindowId" type="u" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="IconPixmap" type="a(iiay)" access="read"/>
    <property name="OverlayIconName" type="s" access="read"/>
    <property name="AttentionIconName" type="s" access="read"/>
    <property name="AttentionIconPixmap" type="a(iiay)" access="read"/>
    <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
    <property name="IconThemePath" type="s" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <method name="ContextMenu">
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
    </method>
    <method name="Activate">
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
    </method>
    <method name="SecondaryActivate">
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
    </method>
    <method name="Scroll">
      <arg name="delta" type="i" direction="in"/>
      <arg name="orientation" type="s" direction="in"/>
    </method>
    <signal name="NewTitle"/>
    <signal name="NewIcon"/>
    <signal name="NewAttentionIcon"/>
    <signal name="NewOverlayIcon"/>
    <signal name="NewToolTip"/>
    <signal name="NewStatus">
      <arg name="status" type="s"/>
    </signal>
  </interface>
</node>
"#;

const DBUS_MENU_XML: &str = r#"
<node>
  <interface name="com.canonical.dbusmenu">
    <property name="Version" type="u" access="read"/>
    <property name="TextDirection" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconThemePath" type="as" access="read"/>
    <method name="GetLayout">
      <arg name="parentId" type="i" direction="in"/>
      <arg name="recursionDepth" type="i" direction="in"/>
      <arg name="propertyNames" type="as" direction="in"/>
      <arg name="revision" type="u" direction="out"/>
      <arg name="layout" type="(ia{sv}av)" direction="out"/>
    </method>
    <method name="GetGroupProperties">
      <arg name="ids" type="ai" direction="in"/>
      <arg name="propertyNames" type="as" direction="in"/>
      <arg name="properties" type="a(ia{sv})" direction="out"/>
    </method>
    <method name="GetProperty">
      <arg name="id" type="i" direction="in"/>
      <arg name="name" type="s" direction="in"/>
      <arg name="value" type="v" direction="out"/>
    </method>
    <method name="Event">
      <arg name="id" type="i" direction="in"/>
      <arg name="eventId" type="s" direction="in"/>
      <arg name="data" type="v" direction="in"/>
      <arg name="timestamp" type="u" direction="in"/>
    </method>
    <method name="EventGroup">
      <arg name="events" type="a(isvu)" direction="in"/>
      <arg name="idErrors" type="ai" direction="out"/>
    </method>
    <method name="AboutToShow">
      <arg name="id" type="i" direction="in"/>
      <arg name="needUpdate" type="b" direction="out"/>
    </method>
    <method name="AboutToShowGroup">
      <arg name="ids" type="ai" direction="in"/>
      <arg name="updatesNeeded" type="ai" direction="out"/>
      <arg name="idErrors" type="ai" direction="out"/>
    </method>
    <signal name="ItemsPropertiesUpdated">
      <arg name="updatedProps" type="a(ia{sv})"/>
      <arg name="removedProps" type="a(ias)"/>
    </signal>
    <signal name="LayoutUpdated">
      <arg name="revision" type="u"/>
      <arg name="parent" type="i"/>
    </signal>
    <signal name="ItemActivationRequested">
      <arg name="id" type="i"/>
      <arg name="timestamp" type="u"/>
    </signal>
  </interface>
</node>
"#;
