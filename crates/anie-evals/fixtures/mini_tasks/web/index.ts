type Task = { title: string; done: boolean };

export function renderTasks(tasks: Record<string, Task>): string {
  return Object.entries(tasks)
    .map(([slug, task]) => `<li data-slug="${slug}">${task.title}${task.done ? " ✓" : ""}</li>`)
    .join("\n");
}
