//! Progress renderer that consumes JSON events and displays them via indicatif
//! This implements the JSON-first architecture where all progress flows through
//! JSON events before being rendered visually.

use anyhow::Result;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::progress_jsonl::{Event, SubTaskBytes, SubTaskStep};

#[derive(Debug, Clone)]
pub enum ProgressFilter {
    /// Show all progress events
    All,
    /// Only show tasks matching these patterns
    TasksMatching(Vec<String>),
}

impl Default for ProgressFilter {
    fn default() -> Self {
        Self::All
    }
}

/// Renders JSON progress events as indicatif progress bars
/// This bridges the gap between stateless JSON events and stateful visual display
pub struct ProgressRenderer {
    multi: MultiProgress,
    bars: HashMap<String, ProgressBar>,
    subtask_bars: HashMap<String, ProgressBar>,
    active_tasks: HashSet<String>,
    current_task_type: Option<String>,
    filter: ProgressFilter,

    // Style templates
    bytes_style: ProgressStyle,
    steps_style: ProgressStyle,
    subtask_style: ProgressStyle,
}

impl ProgressRenderer {
    pub fn new(filter: ProgressFilter) -> Self {
        let multi = MultiProgress::new();

        let bytes_style = ProgressStyle::with_template(
            "{spinner:.green} {msg} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta})"
        ).unwrap()
        .with_key("eta", |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
            write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap_or(());
        });

        let steps_style = ProgressStyle::with_template(
            "{spinner:.green} {msg} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len}",
        )
        .unwrap();

        let subtask_style = ProgressStyle::with_template(
            "  {spinner:.yellow} {msg} [{wide_bar:.green/blue}] {bytes}/{total_bytes}",
        )
        .unwrap();

        Self {
            multi,
            bars: HashMap::new(),
            subtask_bars: HashMap::new(),
            active_tasks: HashSet::new(),
            current_task_type: None,
            filter,
            bytes_style,
            steps_style,
            subtask_style,
        }
    }

    /// Process a JSON progress event and update the visual display
    pub fn handle_event(&mut self, event: &Event<'_>) -> Result<()> {
        match event {
            Event::Start { .. } => {
                // Reset state on start
                self.clear_all_bars();
            }
            Event::ProgressBytes {
                task,
                description,
                id,
                bytes,
                bytes_total,
                steps,
                steps_total,
                subtasks,
                ..
            } => {
                if self.should_render_task(task) {
                    self.ensure_clean_context(task);
                    self.update_bytes_progress(
                        task,
                        description,
                        id,
                        *bytes,
                        *bytes_total,
                        *steps,
                        *steps_total,
                        subtasks,
                    )?;
                }
            }
            Event::ProgressSteps {
                task,
                description,
                id,
                steps,
                steps_total,
                subtasks,
                ..
            } => {
                if self.should_render_task(task) {
                    self.ensure_clean_context(task);
                    self.update_steps_progress(
                        task,
                        description,
                        id,
                        *steps,
                        *steps_total,
                        subtasks,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn should_render_task(&self, task: &str) -> bool {
        match &self.filter {
            ProgressFilter::All => true,
            ProgressFilter::TasksMatching(patterns) => {
                patterns.iter().any(|pattern| task.contains(pattern))
            }
        }
    }

    fn ensure_clean_context(&mut self, task: &str) {
        // For all filters, just track the current task type
        self.current_task_type = Some(task.to_string());
    }

    fn update_bytes_progress(
        &mut self,
        task: &str,
        description: &str,
        id: &str,
        bytes: u64,
        bytes_total: u64,
        steps: u64,
        steps_total: u64,
        subtasks: &[SubTaskBytes<'_>],
    ) -> Result<()> {
        let bar_id = format!("{}:{}", task, id);

        // Check if we need to create a new bar
        let needs_new_bar = !self.bars.contains_key(&bar_id);
        if needs_new_bar {
            let pb = self.multi.add(ProgressBar::new(bytes_total.max(1)));
            pb.set_style(self.bytes_style.clone());
            pb.set_message(format!("{} ({})", description, task));
            self.active_tasks.insert(bar_id.clone());
            self.bars.insert(bar_id.clone(), pb);
        }

        // Get the bar and update it
        let bar = self.bars.get(&bar_id).unwrap();

        // Update main progress
        if bytes_total > 0 {
            bar.set_length(bytes_total);
        }
        bar.set_position(bytes);

        // Show steps if available
        if steps_total > 0 {
            bar.set_message(format!(
                "{} ({}) - Step {}/{}",
                description, task, steps, steps_total
            ));
        }

        // Update or create subtask bars
        self.update_subtask_bars(subtasks, &bar_id)?;

        // Mark as complete if done
        if bytes_total > 0 && bytes >= bytes_total {
            if let Some(bar) = self.bars.get(&bar_id) {
                bar.finish_with_message(format!("✓ {} completed", description));
            }
        }

        Ok(())
    }

    fn update_steps_progress(
        &mut self,
        task: &str,
        description: &str,
        id: &str,
        steps: u64,
        steps_total: u64,
        subtasks: &[SubTaskStep<'_>],
    ) -> Result<()> {
        let bar_id = format!("{}:{}", task, id);

        // Check if we need to create a new bar
        let needs_new_bar = !self.bars.contains_key(&bar_id);
        if needs_new_bar {
            let pb = self.multi.add(ProgressBar::new(steps_total.max(1)));
            pb.set_style(self.steps_style.clone());
            pb.set_message(format!("{} ({})", description, task));
            self.active_tasks.insert(bar_id.clone());
            self.bars.insert(bar_id.clone(), pb);
        }

        // Get the bar and update it
        let bar = self.bars.get(&bar_id).unwrap();

        // Update main progress
        bar.set_length(steps_total);
        bar.set_position(steps);

        // Update subtask display (for steps, we just show completion status)
        for subtask in subtasks {
            let status = if subtask.completed { "✓" } else { "◯" };
            let subtask_id = format!("{}:subtask:{}", bar_id, subtask.id);

            if let Some(subtask_bar) = self.subtask_bars.get(&subtask_id) {
                if subtask.completed {
                    subtask_bar.finish_with_message(format!("  ✓ {}", subtask.description));
                }
            } else if !subtask.completed {
                // Create a spinner for active subtasks
                let subtask_bar = self.multi.insert_after(&bar, ProgressBar::new_spinner());
                subtask_bar
                    .set_style(ProgressStyle::with_template("  {spinner:.yellow} {msg}").unwrap());
                subtask_bar.set_message(format!("{} {}", status, subtask.description));
                subtask_bar.enable_steady_tick(Duration::from_millis(120));
                self.subtask_bars.insert(subtask_id, subtask_bar);
            }
        }

        // Mark as complete if done
        if steps >= steps_total {
            if let Some(bar) = self.bars.get(&bar_id) {
                bar.finish_with_message(format!("✓ {} completed", description));
            }
        }

        Ok(())
    }

    fn update_subtask_bars(
        &mut self,
        subtasks: &[SubTaskBytes<'_>],
        parent_id: &str,
    ) -> Result<()> {
        let parent_bar = self.bars.get(parent_id).cloned();

        for subtask in subtasks {
            let subtask_id = format!("{}:subtask:{}", parent_id, subtask.id);

            let subtask_bar = self
                .subtask_bars
                .entry(subtask_id.clone())
                .or_insert_with(|| {
                    let pb = if let Some(ref parent) = parent_bar {
                        self.multi
                            .insert_after(parent, ProgressBar::new(subtask.bytes_total.max(1)))
                    } else {
                        self.multi.add(ProgressBar::new(subtask.bytes_total.max(1)))
                    };
                    pb.set_style(self.subtask_style.clone());
                    pb.set_message(format!("{}: {}", subtask.description, subtask.id));
                    pb
                });

            // Update subtask progress
            if subtask.bytes_total > 0 {
                subtask_bar.set_length(subtask.bytes_total);
            }
            subtask_bar.set_position(subtask.bytes);

            // Mark as complete if done
            if subtask.bytes_total > 0 && subtask.bytes >= subtask.bytes_total {
                subtask_bar
                    .finish_with_message(format!("  ✓ {}: {}", subtask.description, subtask.id));
            }
        }

        Ok(())
    }

    fn clear_all_bars(&mut self) {
        // Finish all active bars
        for bar in self.bars.values() {
            bar.finish_and_clear();
        }
        for bar in self.subtask_bars.values() {
            bar.finish_and_clear();
        }

        self.bars.clear();
        self.subtask_bars.clear();
        self.active_tasks.clear();
    }

    /// Finish and clean up all progress bars
    pub fn finish(&mut self) {
        self.clear_all_bars();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress_jsonl::{Event, SubTaskBytes};
    use std::borrow::Cow;

    #[test]
    fn test_filter_matching() {
        let mut renderer =
            ProgressRenderer::new(ProgressFilter::TasksMatching(vec!["pulling".to_string()]));

        // Should render pulling tasks
        assert!(renderer.should_render_task("pulling"));
        assert!(renderer.should_render_task("pulling_layers"));

        // Should not render other tasks
        assert!(!renderer.should_render_task("installing"));
        assert!(!renderer.should_render_task("upgrading"));
    }

    #[test]
    fn test_last_task_only() {
        let mut renderer = ProgressRenderer::new(ProgressFilter::LastTaskOnly);

        // All tasks should be renderable
        assert!(renderer.should_render_task("pulling"));
        assert!(renderer.should_render_task("installing"));

        // But ensure_clean_context should clear when task changes
        renderer.ensure_clean_context("pulling");
        assert_eq!(renderer.current_task_type, Some("pulling".to_string()));

        renderer.ensure_clean_context("installing");
        assert_eq!(renderer.current_task_type, Some("installing".to_string()));
    }

    #[tokio::test]
    async fn test_progress_rendering() -> Result<()> {
        let mut renderer = ProgressRenderer::new(ProgressFilter::All);

        // Test bytes progress
        let event = Event::ProgressBytes {
            task: Cow::Borrowed("pulling"),
            description: Cow::Borrowed("Pulling container image"),
            id: Cow::Borrowed("example.com/image:latest"),
            bytes_cached: 0,
            bytes: 1024,
            bytes_total: 4096,
            steps_cached: 0,
            steps: 1,
            steps_total: 3,
            subtasks: vec![SubTaskBytes {
                subtask: Cow::Borrowed("layer"),
                description: Cow::Borrowed("Layer"),
                id: Cow::Borrowed("sha256:abc123"),
                bytes_cached: 0,
                bytes: 512,
                bytes_total: 1024,
            }],
        };

        renderer.handle_event(&event)?;

        // Verify bars were created
        assert!(!renderer.bars.is_empty());
        assert!(!renderer.subtask_bars.is_empty());

        Ok(())
    }
}
