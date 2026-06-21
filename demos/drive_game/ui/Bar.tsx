export type BarSide = "left" | "right";

export type BarProps = {
  label: string;
  value: () => number;
  side: () => BarSide;
};

function signedPercent(value: number): number {
  return Math.round(Math.max(-1, Math.min(1, value)) * 100);
}

function magnitude(value: number): number {
  return Math.min(1, Math.abs(value));
}

export function Bar(props: BarProps) {
  const pct = () => Math.round(magnitude(props.value()) * 100);
  const fillStyle = () => ({ width: pct() + "%" });
  const fillClass = () => "bar-fill " + props.side();

  return (
    <div class="bar-row">
      <div class="bar-label">{props.label}</div>
      <div class="bar-track">
        <div class={fillClass()} style={fillStyle()}></div>
      </div>
      <div class="bar-value">{() => signedPercent(props.value())}%</div>
    </div>
  );
}
