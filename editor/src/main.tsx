import ReactDOM from "react-dom/client";
import "maplibre-gl/dist/maplibre-gl.css";
import App from "./App";

// No <React.StrictMode>: it double-invokes mount effects in dev (mount → cleanup → mount again),
// which churns through two real maplibre-gl `Map` instances (each owning a WebGL context) on every
// mount. maplibre-gl isn't built to be torn down and recreated like that — the first instance's
// context gets lost mid-teardown, corrupting `loaded()` state and layer setup for the survivor
// (symptom: cut-point/line updates silently stop working after the first bbox selection).
ReactDOM.createRoot(document.getElementById("root")!).render(<App />);
