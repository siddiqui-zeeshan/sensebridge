"use strict";

// The window. It shows what the engine reports and sends back what the user changes;
// everything else lives in Rust.
//
// Opened in a plain browser (no Tauri) it runs against a small fake engine, picked
// with ?demo=off|waiting|wired|driver|attaching|ready|live|away, for working on the
// design without a controller.

const $ = (id) => document.getElementById(id);

const RESPONSE_HINTS = {
  fast: "Lowest delay. Best when the controller is close to the PC.",
  balanced: "A touch more delay, fewer dropouts on a busy Bluetooth link.",
  smooth: "Most forgiving of a weak signal, with the most delay.",
};

// ---------------------------------------------------------------------------
// What to say for each engine state

function describe(view) {
  const { running, engine, status } = view;
  if (!running) {
    return {
      tone: "off",
      title: "Off",
      text: "Turn on to give games haptics and adaptive triggers over Bluetooth.",
    };
  }
  const { state, reason, detail } = engine || { state: "starting" };
  switch (state) {
    case "waiting":
      if (reason === "wired") {
        return {
          tone: "wait",
          title: "Controller is on a cable",
          text: "Unplug it to use Bluetooth. Over a cable, games already get haptics.",
        };
      }
      if (reason === "busy") {
        return {
          tone: "warn",
          title: "Can't open the controller",
          text: "Another program may be holding it. Close other controller tools.",
        };
      }
      return {
        tone: "wait",
        title: "Waiting for your controller",
        text: "Press the PS button to turn it on.",
      };
    case "no_driver":
      return {
        tone: "warn",
        title: "One more thing to install",
        text: "Windows needs the free usbip-win2 driver to see a wired controller. Install it and this page carries on by itself.",
        action: { label: "Get the driver", link: "driver" },
      };
    case "attaching":
      if (reason === "failed") {
        return {
          tone: "warn",
          title: "Couldn't connect to Windows",
          text: (detail || "The usbip-win2 client refused.") + " Trying again.",
        };
      }
      return { tone: "busy", title: "Connecting to Windows", text: "Adding a wired DualSense." };
    case "error":
      if (reason === "port") {
        return {
          tone: "warn",
          title: "Another copy is running",
          text: "Quit the other SenseBridge from its tray icon. This one carries on as soon as it's gone.",
        };
      }
      return { tone: "warn", title: "Couldn't start", text: detail || "Something went wrong." };
    case "ready":
      if (status && !status.controller) {
        return {
          tone: "wait",
          title: "Controller disconnected",
          text: "Press the PS button. Your game picks up where it left off.",
        };
      }
      if (status && status.haptics_rate > 0) {
        return { tone: "live", title: "Haptics live", text: "Your game is driving the controller." };
      }
      return {
        tone: "ready",
        title: "Ready",
        text: "Start a game. On Steam, turn off Steam Input for that game.",
      };
    default:
      return { tone: "busy", title: "Starting", text: "Looking for your controller." };
  }
}

function controllerText(view) {
  const { running, engine, status } = view;
  if (!running || !engine) return [false, "Not found"];
  if (engine.state === "waiting") return [false, reasonText(engine.reason)];
  if (!status || !status.controller) return [false, "Offline"];
  if (status.battery == null) return [true, "Bluetooth"];
  return [true, `${status.battery}%${status.charging ? " charging" : " battery"}`];
}

function reasonText(reason) {
  return reason === "wired" ? "On a cable" : reason === "busy" ? "In use" : "Not found";
}

// ---------------------------------------------------------------------------
// Rendering

const view = { running: false, engine: null, status: null };
let level = 0; // what the orb shows; eases down so short pulses are visible
let pendingPower = false;

function render() {
  const say = describe(view);
  document.body.dataset.tone = say.tone;
  $("title").textContent = say.title;
  $("text").textContent = say.text;

  const power = $("power");
  power.textContent = view.running ? "Turn off" : "Turn on";
  power.dataset.on = String(view.running);
  power.disabled = pendingPower;

  const action = $("action");
  action.hidden = !say.action;
  if (say.action) {
    action.textContent = say.action.label;
    action.dataset.link = say.action.link;
  }

  const [controllerOn, controller] = controllerText(view);
  setChip("chip-controller", "controller-value", controllerOn, controller);
  const attached = view.running && !!view.status && view.status.attached;
  setChip("chip-windows", "windows-value", attached, attached ? "Attached" : "Waiting");
  const live = attached && view.status.haptics_rate > 0;
  setChip("chip-haptics", "haptics-value", live, live ? "Live" : "Idle");

  $("test").disabled = !controllerOn;
}

function setChip(chip, value, on, text) {
  $(chip).dataset.on = String(on);
  $(value).textContent = text;
}

function animate() {
  const target = view.running && view.status ? view.status.level : 0;
  level = target > level ? target : level * 0.88;
  if (level < 0.005) level = 0;
  $("orb").style.setProperty("--level", level.toFixed(3));
  requestAnimationFrame(animate);
}

function showStrength(percent) {
  const input = $("strength");
  input.value = percent;
  input.style.setProperty("--fill", `${(percent / input.max) * 100}%`);
  $("strength-value").textContent = `${percent}%`;
  $("strength-hint").textContent =
    percent > 100
      ? "Above 100%, the strongest effects can sound rough."
      : percent === 0
        ? "Haptics are muted. Triggers still work."
        : "100% feels like the controller on a cable.";
}

function showResponse(mode) {
  for (const button of document.querySelectorAll("[data-response]")) {
    button.setAttribute("aria-checked", String(button.dataset.response === mode));
  }
  $("response-hint").textContent = RESPONSE_HINTS[mode];
}

function onEngineEvent(event) {
  if (event.event === "state") {
    view.engine = event;
    if (event.state === "stopped") {
      view.running = false;
      view.status = null;
    }
  } else if (event.event === "status") {
    view.status = event;
  } else {
    return;
  }
  render();
}

// ---------------------------------------------------------------------------
// Talking to the app

function tauriBackend() {
  const { invoke } = window.__TAURI__.core;
  window.__TAURI__.event.listen("engine", (message) => onEngineEvent(message.payload));
  return { call: (name, args) => invoke(name, args) };
}

function demoBackend() {
  const demo = new URLSearchParams(location.search).get("demo") || "live";
  const states = {
    off: null,
    waiting: { state: "waiting", reason: "absent" },
    wired: { state: "waiting", reason: "wired" },
    driver: { state: "no_driver" },
    attaching: { state: "attaching", reason: "" },
    ready: { state: "ready" },
    live: { state: "ready" },
    away: { state: "ready" },
  };
  const engine = states[demo] ? { event: "state", reason: "", detail: "", ...states[demo] } : null;
  if (engine && engine.state === "ready") {
    let tick = 0;
    setInterval(() => {
      tick += 1;
      onEngineEvent({
        event: "status",
        controller: demo !== "away",
        attached: true,
        haptics_rate: demo === "live" ? 94 : 0,
        output_rate: 30,
        level: demo === "live" ? Math.abs(Math.sin(tick / 3)) * 0.9 : 0,
        battery: 80,
        charging: false,
      });
    }, 100);
  }
  const settings = { strength: 1, response: "fast", start_on_launch: true, close_to_tray: true };
  return {
    call: async (name) =>
      name === "snapshot"
        ? { settings, running: !!engine, state: engine, open_at_sign_in: false, version: "0.1.0" }
        : name === "test_haptics",
  };
}

const backend = window.__TAURI__ ? tauriBackend() : demoBackend();

async function setPower(on) {
  pendingPower = true;
  view.running = on;
  if (on) view.engine = null;
  render();
  try {
    await backend.call(on ? "start" : "stop");
  } finally {
    pendingPower = false;
    if (!on) {
      view.status = null;
      view.engine = null;
    }
    render();
  }
}

function bind() {
  $("power").addEventListener("click", () => setPower(!view.running));
  $("action").addEventListener("click", (e) => backend.call("open_link", { name: e.target.dataset.link }));
  $("minimize").addEventListener("click", () => backend.call("minimize_window"));
  $("close").addEventListener("click", () => backend.call("close_window"));
  for (const link of document.querySelectorAll(".link")) {
    link.addEventListener("click", () => backend.call("open_link", { name: link.dataset.link }));
  }

  $("strength").addEventListener("input", (e) => {
    const percent = Number(e.target.value);
    showStrength(percent);
    backend.call("set_strength", { value: percent / 100 });
  });

  $("test").addEventListener("click", async () => {
    const button = $("test");
    const felt = await backend.call("test_haptics");
    button.textContent = felt ? "Buzzed" : "No controller";
    setTimeout(() => (button.textContent = "Test"), 1200);
  });

  const responses = [...document.querySelectorAll("[data-response]")];
  responses.forEach((button, index) => {
    button.addEventListener("click", () => {
      showResponse(button.dataset.response);
      backend.call("set_response", { value: button.dataset.response });
    });
    button.addEventListener("keydown", (e) => {
      const step = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
      if (!step) return;
      const next = responses[(index + step + responses.length) % responses.length];
      next.focus();
      next.click();
    });
  });

  $("start-on-launch").addEventListener("change", (e) =>
    backend.call("set_start_on_launch", { value: e.target.checked }));
  $("close-to-tray").addEventListener("change", (e) =>
    backend.call("set_close_to_tray", { value: e.target.checked }));
  $("open-at-sign-in").addEventListener("change", async (e) => {
    try {
      e.target.checked = await backend.call("set_open_at_sign_in", { value: e.target.checked });
    } catch {
      e.target.checked = !e.target.checked;
    }
  });

  // This is an app, not a page: no context menu, no text cursor on labels.
  window.addEventListener("contextmenu", (e) => e.preventDefault());
}

async function init() {
  bind();
  const snapshot = await backend.call("snapshot");
  view.running = snapshot.running;
  view.engine = snapshot.state;
  showStrength(Math.round(snapshot.settings.strength * 100));
  showResponse(snapshot.settings.response);
  $("start-on-launch").checked = snapshot.settings.start_on_launch;
  $("close-to-tray").checked = snapshot.settings.close_to_tray;
  $("open-at-sign-in").checked = snapshot.open_at_sign_in;
  $("version").textContent = `v${snapshot.version}`;
  render();
  requestAnimationFrame(animate);
}

init();
