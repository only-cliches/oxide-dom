import { render } from "solite-runtime";

function App() {
  const targetLabel = globalThis.state.targetLabel || "Pane";
  const textValue = String(globalThis.state.text || "");
  const radioAValue = Boolean(globalThis.state.radioA);

  const renderRows = () => {
    const nodes = [];
    const count = Math.max(1, Number(globalThis.state.rows || 24));
    for (let i = 0; i < count; i++) {
      const stripeClass = i % 2 === 0 ? "row row-even" : "row row-odd";
      nodes.push(
        <div class={stripeClass}>
          {"Row " + (i + 1) + " - hover and scroll me"}
        </div>,
      );
    }
    return nodes;
  };

  return (
    <div class="panel">
      <div class="panel-title">{targetLabel}: Kitchen Sink</div>

      <div class="toolbar">
        <button
          class="btn btn-add"
          onClick={() => {
            const next = Math.max(1, Number(globalThis.state.rows || 24)) + 1;
            globalThis.state.rows = next;
            sendEvent(
              "action",
              JSON.stringify({ type: "rows", target: targetLabel, count: next }),
            );
          }}
        >
          + Add Row
        </button>

        <button
          class="btn btn-clear"
          onClick={() => {
            globalThis.state.rows = 20;
            globalThis.state.text = "";
            globalThis.state.number = "";
            globalThis.state.range = 50;
            globalThis.state.checkboxChecked = false;
            globalThis.state.radioA = false;
            globalThis.state.radioB = false;
            globalThis.state.password = "";
            globalThis.state.selectValue = "";
            sendEvent(
              "action",
              JSON.stringify({ type: "clear", target: targetLabel }),
            );
          }}
        >
          Clear
        </button>
      </div>

      <input
        class="field field-text"
        type="text"
        value={textValue}
        placeholder="Type here..."
        onInput={(event) => {
          globalThis.state.text = event.value;
        }}
      />

      <input
        class="field field-number"
        type="number"
        value={String(globalThis.state.number ?? "")}
        placeholder="Numeric value"
        min="-100"
        max="100"
        step="0.5"
        onInput={(event) => {
          globalThis.state.number = event.value;
        }}
      />

      <input
        class="field field-range"
        type="range"
        min="0"
        max="100"
        step="5"
        value={String(globalThis.state.range ?? 50)}
        onInput={(event) => {
          globalThis.state.range = event.value;
        }}
      />

      <div class="inline-fields">
        <input
          class="field field-checkbox"
          type="checkbox"
          checked={Boolean(globalThis.state.checkboxChecked)}
          onInput={(event) => {
            globalThis.state.checkboxChecked = event.checked;
          }}
        />
        <input
          class="field field-radio"
          type="radio"
          name="sink-mode"
          onInput={(event) => {
            globalThis.state.radioA = event.checked;
            if (event.checked) {
              globalThis.state.radioB = false;
            }
          }}
        />
        <input
          class="field field-radio"
          type="radio"
          name="sink-mode"
          onInput={(event) => {
            globalThis.state.radioB = event.checked;
            if (event.checked) {
              globalThis.state.radioA = false;
            }
          }}
        />
      </div>

      <input
        class="field field-password"
        type="password"
        value={globalThis.state.password || ""}
        placeholder="secret..."
        onInput={(event) => {
          globalThis.state.password = event.value;
        }}
      />

      <img class="bird-img" src="solite-image://birds" />

      <select
        class="field field-select"
        value={globalThis.state.selectValue ?? ""}
        onChange={(event) => {
          globalThis.state.selectValue = event.value;
          sendEvent(
            "select",
            JSON.stringify({ target: targetLabel, value: event.value }),
          );
        }}
      >
        <option value="" disabled selected hidden>Choose..</option>
        <option value="option1">First Option</option>
        <option value="option2">Second Option</option>
        <option value="option3">Third Option</option>
        <option value="option4" disabled>Disabled Option</option>
        <option value="option5">Last Option</option>
      </select>

      <div
        class="rows"
        onWheel={(event) => {
          globalThis.state.wheelCount = Number(globalThis.state.wheelCount || 0) + 1;
          sendEvent(
            "wheel",
            JSON.stringify({ target: targetLabel, deltaY: event.deltaY }),
          );
        }}
        onScroll={(event) => {
          globalThis.state.scrollTop = event.scrollTop;
        }}
      >
        {renderRows}
      </div>

      <div class="status">
        {() =>
          `rows=${Math.max(1, Number(globalThis.state.rows || 24))} wheel=${globalThis.state.wheelCount || 0} scrollTop=${globalThis.state.scrollTop || 0} text="${textValue}" number="${String(globalThis.state.number ?? "")}" range=${String(globalThis.state.range ?? 50)} checkbox=${globalThis.state.checkboxChecked ? "on" : "off"} radioA=${globalThis.state.radioA ? "on" : "off"} radioB=${globalThis.state.radioB ? "on" : "off"} password=${globalThis.state.password || ""} select=${globalThis.state.selectValue ?? ""}`
        }
      </div>
    </div>
  );
}

render(() => App(), __SOL_ROOT__);
