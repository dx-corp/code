//! Register a renderer once; derive catalog, captures, and source links from it.
use crate::{
    Scene,
    review::{self, Capture},
};
use ratatui::{Frame, Terminal, backend::TestBackend, buffer::Buffer};
use std::collections::BTreeMap;

type Renderer = Box<dyn Fn(&Scene) -> Result<Buffer, String>>;
/// One named fixture family. The closure captures typed, caller-owned mock data.
pub struct Story {
    id: String,
    label: String,
    source: String,
    cases: Vec<(u16, u16, u64)>,
    renderer: Renderer,
}
impl Story {
    pub fn new(
        id: &str,
        label: &str,
        source: &str,
        render: impl Fn(&Scene, &mut Frame<'_>) + 'static,
    ) -> Self {
        Self::buffer(id, label, source, move |scene| {
            let mut terminal = Terminal::new(TestBackend::new(scene.width, scene.height))
                .map_err(|e| e.to_string())?;
            terminal
                .draw(|f| render(scene, f))
                .map_err(|e| e.to_string())?;
            Ok(terminal.backend().buffer().clone())
        })
    }
    pub fn buffer(
        id: &str,
        label: &str,
        source: &str,
        render: impl Fn(&Scene) -> Result<Buffer, String> + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            source: source.into(),
            cases: vec![(60, 24, 0)],
            renderer: Box::new(render),
        }
    }
    /// Cross product of sizes and fixed timestamps; no wall clock or sleeps.
    pub fn matrix(mut self, sizes: &[(u16, u16)], times: &[u64]) -> Self {
        self.cases = sizes
            .iter()
            .flat_map(|&(w, h)| times.iter().map(move |&t| (w, h, t)))
            .collect();
        self
    }
}
#[derive(Default)]
pub struct Registry {
    stories: BTreeMap<String, Story>,
}
impl Registry {
    pub fn add(&mut self, story: Story) -> Result<(), String> {
        if story.id.is_empty()
            || !story
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("story ID must contain letters, digits or hyphens".into());
        }
        if self.stories.contains_key(&story.id) {
            return Err(format!("duplicate story: {}", story.id));
        }
        if story.cases.is_empty()
            || story.cases.iter().any(|&(w, h, t)| {
                !(8..=240).contains(&w) || !(3..=100).contains(&h) || t > 86_400_000
            })
        {
            return Err("story needs bounded dimensions and timestamps".into());
        }
        let mut cases = story.cases.clone();
        cases.sort_unstable();
        cases.dedup();
        if cases.len() != story.cases.len() {
            return Err("duplicate story case".into());
        }
        self.stories.insert(story.id.clone(), story);
        Ok(())
    }
    /// Bridge existing catalogs without changing their IDs or capture matrix.
    pub fn import(
        &mut self,
        scenes: Vec<Scene>,
        source: &str,
        render: fn(&Scene) -> Result<Buffer, String>,
    ) -> Result<(), String> {
        let mut groups: BTreeMap<String, Story> = BTreeMap::new();
        for scene in scenes {
            let story = groups.entry(scene.id.clone()).or_insert_with(|| {
                let mut story = Story::buffer(&scene.id, &scene.label, source, render);
                story.cases.clear();
                story
            });
            story.cases.push((scene.width, scene.height, scene.time_ms));
        }
        for story in groups.into_values() {
            self.add(story)?;
        }
        Ok(())
    }
    pub fn scenes(&self) -> Vec<Scene> {
        self.stories
            .values()
            .flat_map(|s| {
                s.cases.iter().map(|&(width, height, time_ms)| Scene {
                    id: s.id.clone(),
                    label: s.label.clone(),
                    width,
                    height,
                    time_ms,
                })
            })
            .collect()
    }
    pub fn render(&self, scene: &Scene) -> Result<Buffer, String> {
        let story = self.stories.get(&scene.id).ok_or("unknown story")?;
        if !(8..=240).contains(&scene.width)
            || !(3..=100).contains(&scene.height)
            || scene.time_ms > 86_400_000
        {
            return Err("unbounded scene".into());
        }
        (story.renderer)(scene)
    }
    pub fn captures(&self) -> Result<Vec<Capture>, String> {
        self.scenes()
            .into_iter()
            .map(|scene| {
                let mut capture = review::from_buffer(scene.clone(), &self.render(&scene)?)?;
                capture.source = self.stories[&scene.id].source.clone();
                Ok(capture)
            })
            .collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn story() -> Story {
        Story::new("menu", "Menu", "example.rs", |_, f| {
            f.render_widget(ratatui::widgets::Paragraph::new("fixture"), f.area());
        })
        .matrix(&[(40, 20), (80, 30)], &[0, 80])
    }
    #[test]
    fn registry_derives_every_case_and_rejects_collisions() {
        let mut registry = Registry::default();
        registry.add(story()).unwrap();
        assert!(registry.add(story()).is_err());
        let captures = registry.captures().unwrap();
        assert_eq!(captures.len(), 4);
        assert!(captures.iter().all(|c| c.source == "example.rs"));
        assert_eq!(
            review::json(&captures).unwrap(),
            review::json(&registry.captures().unwrap()).unwrap()
        );
    }
}
