import { createMemo, createSignal, render } from "solite-runtime";

function TodoApp() {
  const [todos, setTodos] = createSignal([]);
  const [draft, setDraft] = createSignal("");
  const [filter, setFilter] = createSignal("all");
  let nextId = 1;

  const addTodo = () => {
    const text = draft().trim();
    if (!text) return;
    setTodos([...todos(), { id: nextId++, text, done: false }]);
    setDraft("");
  };

  const toggleTodo = (id) => {
    setTodos(
      todos().map((t) =>
        t.id === id ? { id: t.id, text: t.text, done: !t.done } : t,
      ),
    );
  };

  const deleteTodo = (id) => {
    setTodos(todos().filter((t) => t.id !== id));
  };

  const clearCompleted = () => {
    setTodos(todos().filter((t) => !t.done));
  };

  const visible = createMemo(() => {
    const f = filter();
    if (f === "active") return todos().filter((t) => !t.done);
    if (f === "completed") return todos().filter((t) => t.done);
    return todos();
  });

  const total = createMemo(() => todos().length);
  const remaining = createMemo(() => todos().filter((t) => !t.done).length);
  const completedCount = createMemo(() => total() - remaining());

  const emptyMessage = () => {
    const f = filter();
    if (total() === 0) return "Nothing here yet — add your first todo.";
    if (f === "active") return "All caught up. Nice work.";
    if (f === "completed") return "No completed todos yet.";
    return "";
  };

  const renderList = () => {
    const items = visible();
    if (items.length === 0) {
      return <div class="empty-state">{emptyMessage()}</div>;
    }
    return items.map((todo) => (
      <div class={"todo-item" + (todo.done ? " done" : "")}>
        <label class="todo-check">
          <input
            class="todo-checkbox"
            type="checkbox"
            checked={todo.done}
            onInput={() => toggleTodo(todo.id)}
          />
          <span class="todo-text">{todo.text}</span>
        </label>
        <button
          class="delete-btn"
          onClick={() => deleteTodo(todo.id)}
        >
          ×
        </button>
      </div>
    ));
  };

  return (
    <div class="page">
      <div class="card">
        <header class="hero">
          <h1>Todo</h1>
          <p class="subtitle">A tiny list. One step at a time.</p>
        </header>

        <div class="input-group">
          <input
            class="todo-input"
            type="text"
            placeholder="What needs doing?"
            value={draft()}
            onInput={(e) => setDraft(e.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") addTodo();
            }}
          />
          <button class="add-btn" onClick={addTodo}>
            Add
          </button>
        </div>

        <div class="filter-bar">
          <button
            class={filter() === "all" ? "chip active" : "chip"}
            onClick={() => setFilter("all")}
          >
            All <span class="chip-count">{total()}</span>
          </button>
          <button
            class={filter() === "active" ? "chip active" : "chip"}
            onClick={() => setFilter("active")}
          >
            Active <span class="chip-count">{remaining()}</span>
          </button>
          <button
            class={filter() === "completed" ? "chip active" : "chip"}
            onClick={() => setFilter("completed")}
          >
            Done <span class="chip-count">{completedCount()}</span>
          </button>
        </div>

        <div class="todo-list">{renderList}</div>

        <footer class="footer">
          <span class="footer-status">
            {() => `${remaining()} ${remaining() === 1 ? "item" : "items"} left`}
          </span>
          <button
            class={completedCount() > 0 ? "clear-btn" : "clear-btn hidden"}
            onClick={clearCompleted}
          >
            Clear completed
          </button>
        </footer>
      </div>
    </div>
  );
}

render(() => <TodoApp />, __SOL_ROOT__);
