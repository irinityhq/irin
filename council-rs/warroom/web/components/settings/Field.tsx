export function Field({
  label,
  value,
  onChange,
  placeholder,
  hint,
  type = "text",
  disabled = false,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  hint?: string;
  type?: "text" | "password";
  disabled?: boolean;
}) {
  return (
    <div>
      <span className="label">{label}</span>
      <input
        type={type}
        className="input mt-1.5 w-full font-mono text-xs"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        autoComplete={type === "password" ? "off" : undefined}
        disabled={disabled}
      />
      {hint && <p className="text-[10px] text-fg-dim mt-1">{hint}</p>}
    </div>
  );
}
