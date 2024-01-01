//! A lightweight and configurable status bar for penrose
use crate::{core::Draw, Result};
use penrose::{
    core::{State, WindowManager},
    pure::geometry::Rect,
    x::{event::XEvent, Atom, ClientConfig, Prop, WinType, XConn},
    Color, Xid,
};
use std::{collections::HashMap, fmt};
use tracing::{debug, error, info};

pub mod widgets;

use widgets::Widget;

/// The position of a status bar
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Position {
    /// Top of the screen
    Top,
    /// Bottom of the screen
    Bottom,
}

/// Just an ordering of widgets to display somewhere on the screen
#[derive(Debug)]
pub struct StatusBar {
    /// The order of widgets to display. Keys for UIManager widgets.
    pub order: Vec<String>,
    /// The position to display this Status Bar in.
    pub position: Position,
    /// The screen to display the status bar on.
    pub screens: Vec<usize>,
    /// The height of the bar.
    pub h: u32,
}

/// A simple text based status bar that renders a user defined array of [`Widget`]s.
pub struct UIManager<X: XConn> {
    draw: Draw,
    widgets: HashMap<String, Box<dyn Widget<X>>>,
    bars: Vec<StatusBar>,
    /// X window id, width, index of the bar.
    screens: Vec<(Xid, u32, usize, usize)>,
    bg: Color,
    active_screen: usize,
}

impl<X: XConn> fmt::Debug for UIManager<X> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UIManager")
            .field("widgets", &stringify!(self.widgets))
            .field("screens", &self.screens)
            .field("bg", &self.bg)
            .field("active_screen", &self.active_screen)
            .finish()
    }
}

impl<X: XConn> UIManager<X> {
    /// Try to initialise a new empty status bar. Can fail if we are unable to create a
    /// new window for each bar.
    pub fn try_new(
        bg: impl Into<Color>,
        font: &str,
        point_size: u8,
        widgets: HashMap<String, Box<dyn Widget<X>>>,
        bars: Vec<StatusBar>,
    ) -> Result<Self> {
        let bg = bg.into();
        let draw = Draw::new(font, point_size, bg)?;

        Ok(Self {
            draw,
            widgets,
            bars,
            screens: vec![],
            bg,
            active_screen: 0,
        })
    }

    /// Add this [`UIManager`] into the given [`WindowManager`] along with the required
    /// hooks for driving it from the main WindowManager event loop.
    pub fn add_to(self, mut wm: WindowManager<X>) -> WindowManager<X>
    where
        X: 'static,
    {
        wm.state.add_extension(self);
        wm.state.config.compose_or_set_event_hook(event_hook);
        wm.state.config.compose_or_set_manage_hook(manage_hook);
        wm.state.config.compose_or_set_refresh_hook(refresh_hook);
        wm.state.config.compose_or_set_startup_hook(startup_hook);

        wm
    }

    fn init_for_screens(&mut self) -> Result<()> {
        info!("initialising per screen status bar windows");
        let screen_details = self.draw.conn.screen_details()?;

        // Need a screen for valid each bar X physical screen combo
        let mut screens: Vec<(Xid, u32, usize, usize)> = vec![];
        for (bar_i, bar) in self.bars.iter().enumerate() {
            for scrn_i in bar.screens.iter() {
                if let Some(&Rect { x, y, w, h }) = screen_details.get(*scrn_i) {
                    let y = match bar.position {
                        Position::Top => y,
                        Position::Bottom => h - bar.h,
                    };
                    debug!("creating new window");
                    let id = self.draw.new_window(
                        WinType::InputOutput(Atom::NetWindowTypeDock),
                        Rect::new(x, y, w, bar.h),
                        false,
                    )?;
                    debug!(%id, "setting props");
                    let p = Prop::UTF8String(vec!["penrose-statusbar".to_string()]);
                    for atom in &[Atom::NetWmName, Atom::WmName, Atom::WmClass] {
                        self.draw.conn.set_prop(id, atom.as_ref(), p.clone())?;
                    }
                    let data = &[ClientConfig::StackBottom];
                    self.draw.conn.set_client_config(id, data)?;
                    debug!("flushing");
                    self.draw.flush(id)?;

                    screens.push((id, w, bar_i, *scrn_i));
                }
            }
        }
        self.screens = screens;
        Ok(())
    }

    /// Re-render all widgets in this status bar
    pub fn redraw(&mut self) -> Result<()> {
        for (_i, &(id, w, bar_i, scrn_i)) in self.screens.clone().iter().enumerate() {
            let screen_has_focus = self.active_screen == scrn_i;
            let mut ctx = self.draw.context_for(id)?;
            let bar = &self.bars[bar_i];

            let mut extents = Vec::with_capacity(bar.order.len());
            let mut greedy_indices = vec![];

            for (i, k) in bar.order.iter().enumerate() {
                let w = self.widgets.get_mut(k).unwrap();
                extents.push(w.current_extent(&mut ctx, bar.h)?);
                if w.is_greedy() {
                    greedy_indices.push(i)
                }
            }

            let total = extents.iter().map(|(w, _)| w).sum::<u32>();
            let n_greedy = greedy_indices.len();

            if total < w && n_greedy > 0 {
                let per_greedy = (w - total) / n_greedy as u32;
                for i in greedy_indices.iter() {
                    let (w, h) = extents[*i];
                    extents[*i] = (w + per_greedy, h);
                }
            }

            let mut x = 0;
            for (k, (w, _)) in bar.order.iter().zip(extents) {
                let wd = self.widgets.get_mut(k).unwrap();
                wd.draw(&mut ctx, self.active_screen, screen_has_focus, w, bar.h)?;
                x += w;
                ctx.flush();
                ctx.set_x_offset(x as i32);
            }

            self.draw.flush(id)?;
        }

        Ok(())
    }

    fn redraw_if_needed(&mut self) -> Result<()> {
        if self.widgets.values().any(|w| w.require_draw()) {
            self.redraw()?;
            for (id, _, _, _) in self.screens.iter() {
                self.draw.flush(*id)?;
            }
        }

        Ok(())
    }
}

/// Run any widget startup actions and then redraw
pub fn startup_hook<X: XConn + 'static>(state: &mut State<X>, x: &X) -> penrose::Result<()> {
    let s = state.extension::<UIManager<X>>()?;
    let mut ui_man = s.borrow_mut();

    if let Err(e) = ui_man.init_for_screens() {
        error!(%e, "unabled to initialise for screens");
        return Err(penrose::Error::NoScreens);
    }

    info!("running startup widget hooks");
    for w in ui_man.widgets.values_mut() {
        if let Err(e) = w.on_startup(state, x) {
            error!(%e, "error running widget startup hook");
        };
    }

    if let Err(e) = ui_man.redraw() {
        error!(%e, "error redrawing status bar");
    }

    Ok(())
}

/// Run any widget refresh actions and then redraw if needed
pub fn refresh_hook<X: XConn + 'static>(state: &mut State<X>, x: &X) -> penrose::Result<()> {
    let s = state.extension::<UIManager<X>>()?;
    let mut ui_man = s.borrow_mut();

    ui_man.active_screen = state.client_set.current_screen().index();

    for w in ui_man.widgets.values_mut() {
        if let Err(e) = w.on_refresh(state, x) {
            error!(%e, "error running widget refresh hook");
        }
    }

    if let Err(e) = ui_man.redraw_if_needed() {
        error!(%e, "error redrawing status bar");
    }

    Ok(())
}

/// Run any widget event actions and then redraw if needed
pub fn event_hook<X: XConn + 'static>(
    event: &XEvent,
    state: &mut State<X>,
    x: &X,
) -> penrose::Result<bool> {
    use XEvent::{ConfigureNotify, RandrNotify};

    let s = state.extension::<UIManager<X>>()?;
    let mut ui_man = s.borrow_mut();

    if matches!(event, RandrNotify) || matches!(event, ConfigureNotify(e) if e.is_root) {
        info!("screens have changed: recreating status bars");
        let screens: Vec<_> = ui_man.screens.drain(0..).collect();

        for (id, _, _, _) in screens {
            info!(%id, "removing previous status bar");
            if let Err(e) = ui_man.draw.destroy_window_and_surface(id) {
                error!(%e, "error when removing previous status bar state");
            }
        }

        if let Err(e) = ui_man.init_for_screens() {
            error!(%e, "unabled to initialise for screens");
            return Err(penrose::Error::NoScreens);
        }
    }

    ui_man.active_screen = state.client_set.current_screen().index();

    for w in ui_man.widgets.values_mut() {
        if let Err(e) = w.on_event(event, state, x) {
            error!(%e, "error running widget event hook");
        };
    }

    if let Err(e) = ui_man.redraw_if_needed() {
        error!(%e, "error redrawing status bar");
    }

    Ok(true)
}

/// Run any widget on_new_client actions and then redraw if needed
pub fn manage_hook<X: XConn + 'static>(
    id: Xid,
    state: &mut State<X>,
    x: &X,
) -> penrose::Result<()> {
    let s = state.extension::<UIManager<X>>()?;
    let mut ui_man = s.borrow_mut();

    for w in ui_man.widgets.values_mut() {
        if let Err(e) = w.on_new_client(id, state, x) {
            error!(%e, "error running widget manage hook");
        }
    }

    if let Err(e) = ui_man.redraw_if_needed() {
        error!(%e, "error redrawing status bar");
    }

    Ok(())
}
