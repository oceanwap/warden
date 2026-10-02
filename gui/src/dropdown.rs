//! A popover menu: a widget that draws `base` in the layout and, while
//! `open`, a `menu` on top of everything, under the base and left-aligned
//! with it (above it when there is no room below, moved left when it would
//! leave the window).
//!
//! iced has a tooltip (not interactive) and a pick list (a value, not
//! buttons) but no menu, so this is the tooltip's overlay made clickable. A
//! press outside the menu, or Escape, publishes `on_dismiss` and is swallowed
//! (it does not reach what is under the menu). A press on the base is not
//! swallowed: the base's own button toggles the menu.

use iced::advanced::widget::{self, Tree, Widget};
use iced::advanced::{Clipboard, Layout, Shell, layout, mouse, overlay, renderer};
use iced::keyboard::{self, key};
use iced::{Element, Event, Length, Point, Rectangle, Renderer, Size, Theme, Vector};

/// Space between the base and its menu.
const GAP: f32 = 4.0;

pub struct Dropdown<'a, Message> {
    base: Element<'a, Message>,
    menu: Element<'a, Message>,
    open: bool,
    on_dismiss: Option<Message>,
}

impl<'a, Message: Clone> Dropdown<'a, Message> {
    pub fn new(base: impl Into<Element<'a, Message>>, menu: impl Into<Element<'a, Message>>, open: bool) -> Self {
        Dropdown { base: base.into(), menu: menu.into(), open, on_dismiss: None }
    }

    /// What a press outside the menu (or Escape) sends.
    pub fn on_dismiss(mut self, message: Message) -> Self {
        self.on_dismiss = Some(message);
        self
    }
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for Dropdown<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.base), Tree::new(&self.menu)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[&self.base, &self.menu]);
    }

    fn size(&self) -> Size<Length> {
        self.base.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.base.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.base.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.base.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.base.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.base.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.base.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let mut children = tree.children.iter_mut();
        let base_tree = children.next()?;
        let menu_tree = children.next()?;
        let base = self.base.as_widget_mut().overlay(base_tree, layout, renderer, viewport, translation);
        let menu = self.open.then(|| {
            overlay::Element::new(Box::new(Menu {
                menu: &mut self.menu,
                tree: menu_tree,
                anchor: Rectangle::new(layout.position() + translation, layout.bounds().size()),
                on_dismiss: self.on_dismiss.clone(),
            }))
        });
        if base.is_none() && menu.is_none() {
            return None;
        }
        Some(overlay::Group::with_children(base.into_iter().chain(menu).collect()).overlay())
    }
}

impl<'a, Message: Clone + 'a> From<Dropdown<'a, Message>> for Element<'a, Message> {
    fn from(d: Dropdown<'a, Message>) -> Self {
        Element::new(d)
    }
}

struct Menu<'a, 'b, Message> {
    menu: &'b mut Element<'a, Message>,
    tree: &'b mut Tree,
    /// The base, in window coordinates.
    anchor: Rectangle,
    on_dismiss: Option<Message>,
}

impl<Message: Clone> overlay::Overlay<Message, Theme, Renderer> for Menu<'_, '_, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        let node = self.menu.as_widget_mut().layout(self.tree, renderer, &layout::Limits::new(Size::ZERO, bounds));
        let size = node.size();
        let mut x = self.anchor.x;
        let mut y = self.anchor.y + self.anchor.height + GAP;
        if y + size.height > bounds.height {
            // No room below: above the base, and at the top edge at worst.
            y = (self.anchor.y - GAP - size.height).max(0.0);
        }
        if x + size.width > bounds.width {
            x = (bounds.width - size.width).max(0.0);
        }
        node.move_to(Point::new(x, y))
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.menu.as_widget().draw(
            self.tree,
            renderer,
            theme,
            style,
            layout,
            cursor,
            &Rectangle::with_size(Size::INFINITE),
        );
    }

    fn operate(&mut self, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn widget::Operation) {
        self.menu.as_widget_mut().operate(self.tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        self.menu.as_widget_mut().update(
            self.tree,
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            &layout.bounds(),
        );
        if shell.is_event_captured() {
            return;
        }
        let dismiss = match event {
            // A press on the base is the base's to handle (its button toggles the menu).
            Event::Mouse(mouse::Event::ButtonPressed(_)) => {
                cursor.position_over(layout.bounds()).is_none() && cursor.position_over(self.anchor).is_none()
            }
            Event::Touch(iced::touch::Event::FingerPressed { position, .. }) => {
                !layout.bounds().contains(*position) && !self.anchor.contains(*position)
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Named(key::Named::Escape), .. }) => true,
            _ => false,
        };
        if dismiss {
            if let Some(m) = &self.on_dismiss {
                shell.publish(m.clone());
            }
            shell.capture_event();
        }
    }

    fn mouse_interaction(&self, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &Renderer) -> mouse::Interaction {
        self.menu.as_widget().mouse_interaction(self.tree, layout, cursor, &layout.bounds(), renderer)
    }
}
