const config = {
  content: ["./app/**/*.{ts,tsx}", "./components/**/*.{ts,tsx}"],
  darkMode: "class",
  theme: {
    extend: {
      colors: {
        bg: {
          DEFAULT: "#000000",
          elevated: "#050505",
          overlay: "#0a0a0a",
          deep: "#000000",
        },
        border: {
          DEFAULT: "#1a1a1a",
          bright: "#2a2a2a",
          glow: "#3a3a48",
        },
        fg: {
          DEFAULT: "#eef2f5",
          muted: "#a8b0ba",
          dim: "#6b7280",
          bright: "#eef2f5",
        },
        amber: {
          DEFAULT: "#e5a33a",
          glow: "rgba(229, 163, 58, 0.25)",
          dim: "#9a7028",
        },
        cyan: {
          DEFAULT: "#4fd8e8",
          glow: "rgba(79, 216, 232, 0.2)",
          dim: "#2a7a88",
        },
        magenta: {
          DEFAULT: "#9a5a72",
          glow: "rgba(154, 90, 114, 0.2)",
        },
        success: { DEFAULT: "#6ecf8a", glow: "rgba(110, 207, 138, 0.2)" },
        warning: { DEFAULT: "#c9a227", glow: "rgba(201, 162, 39, 0.2)" },
        danger: { DEFAULT: "#d4635c", glow: "rgba(212, 99, 92, 0.2)" },
      },
      fontFamily: {
        mono: ["var(--font-mono)", "IBM Plex Mono", "ui-monospace", "monospace"],
        sans: ["var(--font-sans)", "Inter", "system-ui", "sans-serif"],
        display: ["var(--font-sans)", "Inter", "system-ui", "sans-serif"],
        authority: ["var(--font-authority)", "Newsreader", "Georgia", "serif"],
      },
      keyframes: {
        "pulse-cyan": {
          "0%, 100%": { opacity: "1" },
          "50%": { opacity: "0.85" },
        },
        "pulse-amber": {
          "0%, 100%": { opacity: "1" },
          "50%": { opacity: "0.85" },
        },
        "pulse-magenta": {
          "0%, 100%": { opacity: "1" },
          "50%": { opacity: "0.9" },
        },
        "scan-line": {
          "0%": { transform: "translateY(-100%)" },
          "100%": { transform: "translateY(100%)" },
        },
        cursor: {
          "0%, 49%": { opacity: "1" },
          "50%, 100%": { opacity: "0" },
        },
        "slide-up": {
          from: { opacity: "0", transform: "translateY(8px)" },
          to: { opacity: "1", transform: "translateY(0)" },
        },
        "live-pulse": {
          "0%, 100%": { opacity: "0.35" },
          "50%": { opacity: "1" },
        },
      },
      animation: {
        "pulse-cyan": "pulse-cyan 2.4s ease-in-out infinite",
        "pulse-amber": "pulse-amber 2.4s ease-in-out infinite",
        "pulse-magenta": "pulse-magenta 2.4s ease-in-out infinite",
        "scan-line": "scan-line 4s linear infinite",
        cursor: "cursor 1s steps(2) infinite",
        "slide-up": "slide-up 0.4s ease-out",
        "live-pulse": "live-pulse 1.2s ease-in-out infinite",
      },
      backgroundImage: {
        "grid-faint": "none",
        "amber-radial": "none",
        "cyan-radial": "none",
      },
    },
  },
  plugins: [],
};

export default config;
