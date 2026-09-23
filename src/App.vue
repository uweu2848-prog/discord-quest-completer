<script setup>
import { ref, computed, watch, nextTick, onMounted, onBeforeUnmount } from 'vue'
import { invoke } from '@tauri-apps/api/tauri'
import { appWindow } from '@tauri-apps/api/window'

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------
const ORB_C = 2 * Math.PI * 108
const DIAL_SEGMENTS = 90
const SEG = ORB_C / DIAL_SEGMENTS
const DIAL_DASH = `${(SEG * 0.72).toFixed(3)} ${(SEG * 0.28).toFixed(3)}`
const PAGE = 100
const PRESETS = [15, 30, 60, 120]
const ACCENTS = [
  { id: 'crimson', name: 'Crimson', color: '#ff2b4a' },
  { id: 'blood', name: 'Blood', color: '#d4142a' },
  { id: 'ember', name: 'Ember', color: '#ff5a1f' },
  { id: 'rose', name: 'Rose', color: '#ff4d8d' },
]

// ---------------------------------------------------------------------------
// Persistence (localStorage, everything is optional and fails quietly)
// ---------------------------------------------------------------------------
const STORE = 'orb-completer:v2'

function load(key, fallback) {
  try {
    const raw = localStorage.getItem(`${STORE}:${key}`)
    return raw ? JSON.parse(raw) : fallback
  } catch (e) {
    return fallback
  }
}

function save(key, value) {
  try {
    localStorage.setItem(`${STORE}:${key}`, JSON.stringify(value))
  } catch (e) {
    // storage full or unavailable, nothing to do
  }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
const games = ref([])
const loading = ref(true)
const query = ref('')
const tab = ref('all')
const limit = ref(PAGE)
const cursor = ref(0)
const searchFocused = ref(false)

const picked = ref([])
const minutes = ref(load('minutes', 15))
const busy = ref(false)
const error = ref('')

const token = ref(load('discord_token', ''))
const showToken = ref(false)
const currentQuest = ref(null)
const autoStartEnabled = ref(load('auto_start', true))
const autoWatchEnabled = ref(load('auto_watch', false))

const quests = ref([])
const finished = ref([])
const focusId = ref(null)
const discordRunning = ref(false)
const confirmRestart = ref(false)
const settingsOpen = ref(false)
const failedIcons = ref({})

const favorites = ref(load('favorites', []))
const recents = ref(load('recents', []))
const settings = ref({ sound: true, compact: false, accent: 'crimson', autoStart: true, autoWatch: false, ...load('settings', {}) })
const stats = ref({ quests: 0, minutes: 0, ...load('stats', {}) })

const searchEl = ref(null)
const listEl = ref(null)

// last known status per running quest, used to tell "finished" from "stopped"
const lastSeen = new Map()
const rateLimitUntil = ref(0)
const automationState = ref({
  currentQuest: null,
  autoStart: false,
  autoWatch: false,
  status: 'idle',
  cooldownUntil: 0,
})
const automationLog = ref([])
const apiDebug = ref([])
let statusTimer = null
let discordTimer = null
let confirmTimer = null
let audioCtx = null

// ---------------------------------------------------------------------------
// Derived
// ---------------------------------------------------------------------------
const byId = computed(() => new Map(games.value.map((g) => [String(g.id), g])))
const favSet = computed(() => new Set(favorites.value.map(String)))

// Build the running-id set from a string so the game list does not re-render
// every second when the quest timers tick but the set of running games is unchanged.
const runningKey = computed(() =>
  quests.value
    .map((q) => String(q.id))
    .sort()
    .join(',')
)
const runningSet = computed(() => new Set(runningKey.value ? runningKey.value.split(',') : []))

const tabs = computed(() => [
  { id: 'all', label: 'All', count: 0 },
  { id: 'favorites', label: 'Favorites', count: favorites.value.length },
  { id: 'recent', label: 'Recent', count: recents.value.length },
  { id: 'running', label: 'Running', count: quests.value.length },
])

const pool = computed(() => {
  const lookup = (id) => byId.value.get(String(id))
  switch (tab.value) {
    case 'favorites':
      return favorites.value
        .map(lookup)
        .filter(Boolean)
        .sort((a, b) => a.name.localeCompare(b.name))
    case 'recent':
      return recents.value.map(lookup).filter(Boolean)
    case 'running':
      return quests.value.map((q) => lookup(q.id) || { id: q.id, name: q.name, icon: null })
    default:
      return games.value
  }
})

// Names that start with the search come first, then names that contain it.
const matches = computed(() => {
  const q = query.value.trim().toLowerCase()
  if (!q) return pool.value
  const starts = []
  const contains = []
  for (const g of pool.value) {
    const n = g.name.toLowerCase()
    if (n.startsWith(q)) starts.push(g)
    else if (n.includes(q)) contains.push(g)
  }
  return starts.concat(contains)
})
const visible = computed(() => matches.value.slice(0, limit.value))

const emptyText = computed(() => {
  if (query.value.trim()) return `No games match “${query.value.trim()}”. Try a shorter name.`
  if (tab.value === 'favorites') return 'No favorites yet. Star a game to keep it here.'
  if (tab.value === 'recent') return 'Nothing here yet. Games you start show up here.'
  if (tab.value === 'running') return 'No quests are running.'
  return 'No games found.'
})

const focus = computed(
  () => quests.value.find((q) => q.id === focusId.value) || quests.value[0] || null
)
const remaining = computed(() =>
  focus.value ? Math.max(focus.value.total_secs - focus.value.elapsed_secs, 0) : 0
)
const orbOffset = computed(() => ORB_C * (1 - (focus.value ? progress(focus.value) : 0)))
const finishAt = computed(() => (focus.value ? clock(Date.now() + remaining.value * 1000) : ''))
const allDoneAt = computed(() => {
  if (quests.value.length < 2) return ''
  const longest = Math.max(...quests.value.map(timeLeft))
  return clock(Date.now() + longest * 1000)
})

const currentQuestLabel = computed(() => {
  if (currentQuest.value && currentQuest.value.targeted_game) {
    return currentQuest.value.targeted_game.name
  }
  return 'No active Discord quest'
})

const cooldownRemaining = computed(() => {
  const left = Math.max(0, Math.ceil((rateLimitUntil.value - Date.now()) / 1000))
  return left
})

const startLabel = computed(() => {
  if (picked.value.length > 1) return `Start ${picked.value.length} quests`
  return picked.value[0] && isRunning(picked.value[0].id) ? 'Restart quest' : 'Start quest'
})

const windowTitle = computed(() =>
  focus.value ? `${fmt(remaining.value)} left: ${focus.value.name}` : 'Orb Completer'
)

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
function iconSrc(g) {
  if (!g || !g.icon || failedIcons.value[g.id]) return null
  return `https://cdn.discordapp.com/app-icons/${g.id}/${g.icon}.png?size=64`
}

function hue(name) {
  let h = 0
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) % 360
  return h
}

function tileStyle(g) {
  const h = hue(g.name)
  return {
    background: `linear-gradient(135deg, hsl(${h} 34% 32%), hsl(${(h + 40) % 360} 38% 17%))`,
  }
}

function initials(name) {
  const words = name.split(/[^\p{L}\p{N}]+/u).filter(Boolean)
  if (!words.length) return '?'
  const first = Array.from(words[0])[0]
  const second = words[1] ? Array.from(words[1])[0] : ''
  return (first + second).toUpperCase()
}

function fmt(sec) {
  const m = Math.floor(sec / 60)
  const s = sec % 60
  return `${m}:${String(s).padStart(2, '0')}`
}

function fmtHours(min) {
  if (min < 60) return `${min}m`
  return `${Math.floor(min / 60)}h ${min % 60}m`
}

function clock(ms) {
  return new Date(ms).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })
}

function progress(q) {
  return q.total_secs ? Math.min(q.elapsed_secs / q.total_secs, 1) : 0
}

function percent(q) {
  return Math.floor(progress(q) * 100)
}

function timeLeft(q) {
  return Math.max(q.total_secs - q.elapsed_secs, 0)
}

function isRunning(id) {
  return runningSet.value.has(String(id))
}

function isPicked(id) {
  return picked.value.some((p) => String(p.id) === String(id))
}

function isFav(id) {
  return favSet.value.has(String(id))
}

function safeMinutes() {
  const n = Math.round(Number(minutes.value))
  if (!Number.isFinite(n)) return 15
  return Math.min(Math.max(n, 1), 240)
}

// ---------------------------------------------------------------------------
// Selection, favorites, recents
// ---------------------------------------------------------------------------
function choose(g, additive) {
  const i = picked.value.findIndex((p) => String(p.id) === String(g.id))
  if (additive) {
    if (i >= 0) picked.value.splice(i, 1)
    else picked.value.push(g)
  } else {
    picked.value = [g]
  }
}

function onRowClick(g, e) {
  const additive = e.ctrlKey || e.metaKey || e.shiftKey
  if (!additive && picked.value.length === 1 && isPicked(g.id)) picked.value = []
  else choose(g, additive)
}

function toggleFav(id) {
  const key = String(id)
  favorites.value = favSet.value.has(key)
    ? favorites.value.filter((x) => String(x) !== key)
    : [...favorites.value, key]
}

function pushRecent(id) {
  const key = String(id)
  recents.value = [key, ...recents.value.filter((x) => String(x) !== key)].slice(0, 12)
}

// ---------------------------------------------------------------------------
// Sound
// ---------------------------------------------------------------------------
function warmAudio() {
  try {
    audioCtx = audioCtx || new (window.AudioContext || window.webkitAudioContext)()
    if (audioCtx.state === 'suspended') audioCtx.resume()
  } catch (e) {
    // audio not available
  }
}

function chime() {
  if (!settings.value.sound) return
  try {
    warmAudio()
    if (!audioCtx) return
    const now = audioCtx.currentTime
    ;[660, 880, 1320].forEach((freq, i) => {
      const t = now + i * 0.12
      const osc = audioCtx.createOscillator()
      const gain = audioCtx.createGain()
      osc.type = 'sine'
      osc.frequency.value = freq
      gain.gain.setValueAtTime(0.0001, t)
      gain.gain.exponentialRampToValueAtTime(0.16, t + 0.02)
      gain.gain.exponentialRampToValueAtTime(0.0001, t + 0.35)
      osc.connect(gain)
      gain.connect(audioCtx.destination)
      osc.start(t)
      osc.stop(t + 0.4)
    })
  } catch (e) {
    // ignore
  }
}

// ---------------------------------------------------------------------------
// Backend calls
// ---------------------------------------------------------------------------
async function loadGames() {
  loading.value = true
  error.value = ''
  try {
    recordDebug('fetch_games', 'request', 'loading detectable games')
    games.value = await invoke('fetch_games')
    recordDebug('fetch_games', 'success', `${games.value.length} games loaded`)
  } catch (e) {
    recordDebug('fetch_games', 'error', String(e))
    error.value = `Couldn't load the game list: ${e}`
  } finally {
    loading.value = false
  }
}

async function refreshStatus() {
  try {
    const list = await invoke('quest_status')
    const ids = new Set(list.map((q) => q.id))
    let completed = false

    for (const [id, prev] of lastSeen) {
      if (!ids.has(id)) {
        // it vanished on its own: if it was nearly at the end, it completed
        if (prev.elapsed_secs >= prev.total_secs - 5) {
          finished.value.unshift({ id, name: prev.name, at: Date.now(), key: `${id}-${Date.now()}` })
          stats.value.quests += 1
          stats.value.minutes += Math.round(prev.total_secs / 60)
          completed = true
        }
        lastSeen.delete(id)
      }
    }
    for (const q of list) lastSeen.set(q.id, q)

    if (finished.value.length > 30) finished.value = finished.value.slice(0, 30)
    quests.value = list
    if (completed) chime()
  } catch (e) {
    // transient polling errors are not worth surfacing
  }
}

async function refreshDiscord() {
  try {
    discordRunning.value = await invoke('check_discord_running')
  } catch (e) {
    discordRunning.value = false
  }
}

function addLog(message, level = 'info') {
  automationLog.value = [{ id: `${Date.now()}-${Math.random()}`, level, message, at: new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }) }, ...automationLog.value].slice(0, 8)
}

function recordDebug(action, status, detail = '') {
  const entry = {
    id: `${Date.now()}-${Math.random()}`,
    action,
    status,
    detail,
    at: new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }),
  }
  apiDebug.value = [entry, ...apiDebug.value].slice(0, 10)
}

function setRateLimit(seconds) {
  const delay = Math.max(0, Number(seconds) || 0)
  rateLimitUntil.value = Date.now() + delay * 1000
  automationState.value.cooldownUntil = rateLimitUntil.value
  addLog(`Rate limiting enabled for ${delay}s`, 'warn')
}

async function syncCurrentQuest() {
  const authToken = token.value.trim()
  if (!authToken) {
    currentQuest.value = null
    return null
  }

  try {
    recordDebug('fetch_current_quest', 'request', 'checking active Discord quest')
    const quest = await invoke('fetch_current_quest', { token: authToken })
    currentQuest.value = quest
    automationState.value.currentQuest = quest
    automationState.value.status = quest ? 'active' : 'idle'
    addLog(quest ? `Current quest detected: ${quest.targeted_game?.name || 'Unknown'}` : 'No active quest detected')
    recordDebug('fetch_current_quest', quest ? 'success' : 'idle', quest ? quest.targeted_game?.name || 'Quest found' : 'No active quest')
    return quest
  } catch (e) {
    currentQuest.value = null
    automationState.value.currentQuest = null
    automationState.value.status = 'error'
    addLog(`Quest sync failed: ${e}`, 'error')
    recordDebug('fetch_current_quest', 'error', String(e))
    return null
  }
}

async function maybeAutoStartMissingQuest() {
  if (!autoStartEnabled.value || !token.value.trim()) return

  const active = await syncCurrentQuest()
  if (active) return

  if (!picked.value.length) return

  await startQuests()
}

async function startQuests() {
  if (!picked.value.length || busy.value || cooldownRemaining.value > 0) return
  busy.value = true
  error.value = ''
  automationState.value.status = 'starting'
  addLog(`Starting ${picked.value.length} quest${picked.value.length > 1 ? 's' : ''}`)
  warmAudio()
  const mins = safeMinutes()
  const failures = []
  let focused = false

  for (const game of picked.value) {
    try {
      recordDebug('start_quest', 'request', `starting ${game.name} for ${mins} minutes`)
      await invoke('start_quest', { game, minutes: mins })
      pushRecent(game.id)
      addLog(`Started quest: ${game.name}`)
      recordDebug('start_quest', 'success', `${game.name} launched`)

      if (autoWatchEnabled.value && token.value.trim()) {
        automationState.value.autoWatch = true
        addLog(`Enabled video progress watcher for ${game.name}`)
        recordDebug('auto_watch_video', 'request', `watcher enabled for ${game.name}`)
        await invoke('auto_watch_video', {
          token: token.value.trim(),
          questId: String(game.id),
          enabled: true,
        })
        recordDebug('auto_watch_video', 'success', `watcher accepted for ${game.name}`)
      }

      if (!focused) {
        focusId.value = game.id
        focused = true
      }
    } catch (e) {
      if (String(e).toLowerCase().includes('rate')) {
        setRateLimit(30)
      }
      recordDebug('start_quest', 'error', `${game.name}: ${String(e)}`)
      addLog(`Failed to start ${game.name}: ${e}`, 'error')
      failures.push(`${game.name} (${e})`)
    }
  }

  if (failures.length) error.value = `Couldn't start ${failures.join(', ')}`
  await refreshStatus()
  await syncCurrentQuest()
  automationState.value.autoStart = autoStartEnabled.value
  automationState.value.autoWatch = autoWatchEnabled.value
  automationState.value.status = currentQuest.value ? 'active' : 'idle'
  busy.value = false
}

async function stopQuest(id) {
  try {
    lastSeen.delete(id)
    await invoke('stop_quest', { gameId: id })
    await refreshStatus()
  } catch (e) {
    error.value = `Couldn't stop that quest: ${e}`
  }
}

async function stopAll() {
  for (const q of [...quests.value]) {
    try {
      lastSeen.delete(q.id)
      await invoke('stop_quest', { gameId: q.id })
    } catch (e) {
      error.value = `Couldn't stop ${q.name}: ${e}`
    }
  }
  await refreshStatus()
}

function onRestartClick() {
  if (!confirmRestart.value) {
    confirmRestart.value = true
    clearTimeout(confirmTimer)
    confirmTimer = setTimeout(() => (confirmRestart.value = false), 4000)
    return
  }
  clearTimeout(confirmTimer)
  confirmRestart.value = false
  restartDiscord()
}

async function restartDiscord() {
  try {
    await invoke('restart_discord')
    setTimeout(refreshDiscord, 4000)
  } catch (e) {
    error.value = `Couldn't restart Discord: ${e}`
  }
}

function resetStats() {
  stats.value = { quests: 0, minutes: 0 }
}

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------
function moveCursor(step) {
  const max = visible.value.length - 1
  if (max < 0) return
  cursor.value = Math.min(Math.max(cursor.value + step, 0), max)
  nextTick(() => {
    const el = listEl.value && listEl.value.querySelector(`[data-i="${cursor.value}"]`)
    if (el) el.scrollIntoView({ block: 'nearest' })
  })
}

function onSearchKey(e) {
  if (e.key === 'ArrowDown') {
    e.preventDefault()
    moveCursor(1)
  } else if (e.key === 'ArrowUp') {
    e.preventDefault()
    moveCursor(-1)
  } else if (e.key === 'Enter' && !(e.ctrlKey || e.metaKey)) {
    const g = visible.value[cursor.value]
    if (g) choose(g, e.shiftKey)
  } else if (e.key === 'Escape') {
    if (query.value) query.value = ''
    else e.target.blur()
  }
}

function onKey(e) {
  const tag = document.activeElement ? document.activeElement.tagName : ''
  const typing = tag === 'INPUT' || tag === 'TEXTAREA'
  const mod = e.ctrlKey || e.metaKey

  if ((e.key === '/' && !typing) || (mod && e.key.toLowerCase() === 'k')) {
    e.preventDefault()
    if (searchEl.value) searchEl.value.focus()
    return
  }
  if (mod && e.key === 'Enter') {
    e.preventDefault()
    startQuests()
    return
  }
  if (e.key === 'Escape') {
    if (settingsOpen.value) settingsOpen.value = false
    else if (!typing && picked.value.length) picked.value = []
  }
}

// ---------------------------------------------------------------------------
// Watchers
// ---------------------------------------------------------------------------
watch([query, tab], () => {
  limit.value = PAGE
  cursor.value = 0
})

watch(favorites, (v) => save('favorites', v), { deep: true })
watch(recents, (v) => save('recents', v), { deep: true })
watch(token, (v) => save('discord_token', v), { deep: true })
watch(autoStartEnabled, (v) => save('auto_start', v), { deep: true })
watch(autoWatchEnabled, (v) => save('auto_watch', v), { deep: true })
watch(settings, (v) => save('settings', v), { deep: true })
watch(stats, (v) => save('stats', v), { deep: true })
watch(minutes, (v) => {
  const n = Number(v)
  if (Number.isFinite(n) && n >= 1) save('minutes', Math.min(Math.round(n), 240))
})

watch(
  () => settings.value.accent,
  (a) => {
    document.documentElement.dataset.accent = a
  },
  { immediate: true }
)

watch(
  () => settings.value.autoStart,
  (value) => {
    autoStartEnabled.value = value
    automationState.value.autoStart = value
  },
  { immediate: true }
)

watch(
  () => settings.value.autoWatch,
  (value) => {
    autoWatchEnabled.value = value
    automationState.value.autoWatch = value
  },
  { immediate: true }
)

watch(autoStartEnabled, (value) => {
  settings.value.autoStart = value
  automationState.value.autoStart = value
})

watch(autoWatchEnabled, (value) => {
  settings.value.autoWatch = value
  automationState.value.autoWatch = value
})

// Shows the countdown in the taskbar. Needs "window": { "setTitle": true } in the
// Tauri allowlist, and is silently skipped if it isn't there.
watch(
  windowTitle,
  (t) => {
    document.title = t
    try {
      appWindow.setTitle(t).catch(() => {})
    } catch (e) {
      // not running inside Tauri
    }
  },
  { immediate: true }
)

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------
onMounted(() => {
  loadGames()
  refreshStatus()
  syncCurrentQuest()
  refreshDiscord()
  statusTimer = setInterval(refreshStatus, 1000)
  discordTimer = setInterval(refreshDiscord, 5000)
  setInterval(() => {
    if (rateLimitUntil.value > 0 && Date.now() >= rateLimitUntil.value) {
      rateLimitUntil.value = 0
      automationState.value.cooldownUntil = 0
    }
  }, 1000)
  window.addEventListener('keydown', onKey)
})

onBeforeUnmount(() => {
  clearInterval(statusTimer)
  clearInterval(discordTimer)
  clearTimeout(confirmTimer)
  window.removeEventListener('keydown', onKey)
})
</script>

<template>
  <div class="app" :class="{ compact: settings.compact }">
    <header class="bar">
      <div class="brand">
        <span class="mark" aria-hidden="true"></span>
        <h1>Drew's Discord Quest Completer.</h1>
      </div>

      <div class="actions">
        <span class="status" :class="discordRunning ? 'on' : 'off'">
          <span class="dot" aria-hidden="true"></span>
          {{ discordRunning ? 'Discord is running' : 'Discord not detected' }}
        </span>
        <button class="btn ghost" :class="{ warn: confirmRestart }" @click="onRestartClick">
          {{ confirmRestart ? 'Click again to confirm' : 'Restart Discord' }}
        </button>
        <button
          class="icon-btn"
          :class="{ lit: settingsOpen }"
          aria-label="Settings"
          :aria-expanded="settingsOpen"
          @click="settingsOpen = !settingsOpen"
        >
          <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true">
            <path d="M3 6h9M16 6h1M3 14h1M8 14h9" />
            <circle cx="14" cy="6" r="2" />
            <circle cx="6" cy="14" r="2" />
          </svg>
        </button>

        <Transition name="pop">
          <div v-if="settingsOpen" class="settings" role="dialog" aria-label="Settings">
            <div class="set-row">
              <div>
                <strong>Completion sound</strong>
                <small>Play a chime when a quest finishes</small>
              </div>
              <button
                class="switch"
                :class="{ on: settings.sound }"
                role="switch"
                :aria-checked="settings.sound"
                aria-label="Completion sound"
                @click="settings.sound = !settings.sound"
              >
                <span></span>
              </button>
            </div>

            <div class="set-row">
              <div>
                <strong>Compact list</strong>
                <small>Fit more games on screen</small>
              </div>
              <button
                class="switch"
                :class="{ on: settings.compact }"
                role="switch"
                :aria-checked="settings.compact"
                aria-label="Compact list"
                @click="settings.compact = !settings.compact"
              >
                <span></span>
              </button>
            </div>

            <div class="set-row stacked">
              <div>
                <strong>Discord token</strong>
                <small>Used only to read current quest state and auto-start safely</small>
              </div>
              <div class="token-row">
                <input
                  class="token-input"
                  v-model="token"
                  :type="showToken ? 'text' : 'password'"
                  placeholder="Paste token"
                  autocomplete="off"
                  spellcheck="false"
                />
                <button class="token-toggle" @click="showToken = !showToken">
                  {{ showToken ? 'Hide' : 'Show' }}
                </button>
              </div>
            </div>

            <div class="set-row">
              <div>
                <strong>Auto-start missing quest</strong>
                <small>Runs only when a Discord token is saved</small>
              </div>
              <button
                class="switch"
                :class="{ on: settings.autoStart }"
                role="switch"
                :aria-checked="settings.autoStart"
                aria-label="Auto-start missing quest"
                @click="settings.autoStart = !settings.autoStart"
              >
                <span></span>
              </button>
            </div>

            <div class="set-row">
              <div>
                <strong>Auto-watch video progress</strong>
                <small>Opt-in, throttled, and rate-limit aware</small>
              </div>
              <button
                class="switch"
                :class="{ on: settings.autoWatch }"
                role="switch"
                :aria-checked="settings.autoWatch"
                aria-label="Auto-watch video progress"
                @click="settings.autoWatch = !settings.autoWatch"
              >
                <span></span>
              </button>
            </div>

            <div class="set-row automation-card">
              <div class="status-summary">
                <strong>Automation controls</strong>
                <small>
                  {{ automationState.status === 'active' ? 'Quest detected' : automationState.status === 'starting' ? 'Starting...' : automationState.status === 'error' ? 'Token or API issue' : 'Idle' }}
                </small>
              </div>
              <div class="automation-grid">
                <div class="automation-box">
                  <span class="label">Current quest</span>
                  <strong>{{ currentQuestLabel }}</strong>
                </div>
                <div class="automation-box">
                  <span class="label">Auto-start</span>
                  <strong>{{ settings.autoStart ? 'On' : 'Off' }}</strong>
                </div>
                <div class="automation-box">
                  <span class="label">Video watcher</span>
                  <strong>{{ settings.autoWatch ? 'On' : 'Off' }}</strong>
                </div>
                <div class="automation-box">
                  <span class="label">Cooldown</span>
                  <strong>{{ cooldownRemaining > 0 ? `${cooldownRemaining}s` : 'Ready' }}</strong>
                </div>
              </div>
            </div>

            <div class="set-row status-panel">
              <div class="status-summary">
                <strong>Automation status</strong>
                <small>
                  {{ automationState.status === 'active' ? 'Quest detected' : automationState.status === 'starting' ? 'Starting...' : automationState.status === 'error' ? 'Token or API issue' : 'Idle' }}
                </small>
              </div>
              <div class="status-meta">
                <span>{{ currentQuestLabel }}</span>
                <span v-if="cooldownRemaining > 0">Cooldown: {{ cooldownRemaining }}s</span>
              </div>
            </div>

            <div class="set-row log-panel">
              <div class="status-summary">
                <strong>Automation log</strong>
              </div>
              <ul class="mini-log">
                <li v-for="entry in automationLog" :key="entry.id" :class="entry.level">
                  <span>{{ entry.at }}</span>
                  <strong>{{ entry.message }}</strong>
                </li>
              </ul>
            </div>

            <div class="set-row debug-panel">
              <div class="status-summary">
                <strong>API debug</strong>
                <small>Request / result trace</small>
              </div>
              <ul class="mini-log debug-log">
                <li v-for="entry in apiDebug" :key="entry.id" :class="entry.status">
                  <span>{{ entry.at }} • {{ entry.action }}</span>
                  <strong>{{ entry.status }}</strong>
                  <small v-if="entry.detail">{{ entry.detail }}</small>
                </li>
              </ul>
            </div>

            <div class="set-row">
              <strong>Accent color</strong>
              <div class="swatches" role="group" aria-label="Accent color">
                <button
                  v-for="a in ACCENTS"
                  :key="a.id"
                  class="swatch"
                  :class="{ on: settings.accent === a.id }"
                  :style="{ background: a.color }"
                  :aria-label="a.name"
                  :aria-pressed="settings.accent === a.id"
                  @click="settings.accent = a.id"
                ></button>
              </div>
            </div>

            <dl class="keys">
              <div><dt><kbd>/</kbd> or <kbd>Ctrl</kbd> <kbd>K</kbd></dt><dd>Search</dd></div>
              <div><dt><kbd>↑</kbd> <kbd>↓</kbd> <kbd>Enter</kbd></dt><dd>Select a game</dd></div>
              <div><dt><kbd>Shift</kbd> <kbd>Enter</kbd></dt><dd>Add to selection</dd></div>
              <div><dt><kbd>Ctrl</kbd> <kbd>Enter</kbd></dt><dd>Start</dd></div>
              <div><dt><kbd>Esc</kbd></dt><dd>Clear</dd></div>
            </dl>
          </div>
        </Transition>
      </div>
    </header>
    <div v-if="settingsOpen" class="scrim" @click="settingsOpen = false"></div>

    <p v-if="error" class="banner" role="alert">
      <span>{{ error }}</span>
      <button class="link" @click="error = ''">Dismiss</button>
    </p>

    <div class="body">
      <!-- Library -->
      <section class="library" aria-label="Games">
        <div class="search">
          <svg class="mag" viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
            <circle cx="8.5" cy="8.5" r="5.5" />
            <path d="M13 13l4 4" stroke-linecap="round" />
          </svg>
          <input
            ref="searchEl"
            v-model="query"
            type="search"
            :placeholder="games.length ? `Search ${games.length.toLocaleString()} games` : 'Search games'"
            :disabled="loading"
            aria-label="Search games"
            @keydown="onSearchKey"
            @focus="searchFocused = true"
            @blur="searchFocused = false"
          />
          <span v-if="query" class="found">{{ matches.length.toLocaleString() }} found</span>
        </div>

        <div class="tabs" role="tablist" aria-label="Game filters">
          <button
            v-for="t in tabs"
            :key="t.id"
            class="tab"
            :class="{ on: tab === t.id }"
            role="tab"
            :aria-selected="tab === t.id"
            @click="tab = t.id"
          >
            {{ t.label }}<span v-if="t.count" class="count">{{ t.count }}</span>
          </button>
        </div>

        <div v-if="loading" class="skeletons" aria-busy="true" aria-label="Loading games">
          <span v-for="n in 8" :key="n" class="sk"></span>
        </div>

        <div v-else-if="!games.length" class="state">
          <p>No games loaded.</p>
          <button class="btn ghost" @click="loadGames">Try again</button>
        </div>

        <p v-else-if="!matches.length" class="state">{{ emptyText }}</p>

        <ul v-else ref="listEl" class="games">
          <li
            v-for="(g, i) in visible"
            :key="g.id"
            class="row"
            :class="{ active: isPicked(g.id), cursor: searchFocused && i === cursor }"
            :data-i="i"
          >
            <button class="game" :aria-pressed="isPicked(g.id)" @click="onRowClick(g, $event)">
              <span class="tile" :style="iconSrc(g) ? null : tileStyle(g)" aria-hidden="true">
                <img v-if="iconSrc(g)" :src="iconSrc(g)" alt="" @error="failedIcons[g.id] = true" />
                <template v-else>{{ initials(g.name) }}</template>
              </span>
              <span class="game-name">{{ g.name }}</span>
              <span v-if="isRunning(g.id)" class="badge"><i aria-hidden="true"></i>Running</span>
            </button>
            <button
              class="star"
              :class="{ on: isFav(g.id) }"
              :aria-pressed="isFav(g.id)"
              :aria-label="isFav(g.id) ? `Remove ${g.name} from favorites` : `Add ${g.name} to favorites`"
              @click="toggleFav(g.id)"
            >
              <svg viewBox="0 0 20 20" aria-hidden="true">
                <path d="M10 2.8l2.2 4.6 5 .7-3.6 3.5.9 5-4.5-2.4-4.5 2.4.9-5L2.8 8.1l5-.7L10 2.8z" />
              </svg>
            </button>
          </li>
          <li v-if="matches.length > visible.length" class="more">
            <button class="btn ghost" @click="limit += PAGE">
              Show {{ Math.min(PAGE, matches.length - visible.length) }} more
            </button>
            <span>{{ visible.length }} of {{ matches.length.toLocaleString() }}</span>
          </li>
        </ul>

        <Transition name="rise">
          <div v-if="picked.length" class="dock">
            <div class="dock-game">
              <span class="stack" aria-hidden="true">
                <span
                  v-for="g in picked.slice(0, 3)"
                  :key="g.id"
                  class="tile"
                  :style="iconSrc(g) ? null : tileStyle(g)"
                >
                  <img v-if="iconSrc(g)" :src="iconSrc(g)" alt="" />
                  <template v-else>{{ initials(g.name) }}</template>
                </span>
                <span v-if="picked.length > 3" class="tile plus">+{{ picked.length - 3 }}</span>
              </span>
              <span class="dock-text">
                <strong>{{ picked.length === 1 ? picked[0].name : `${picked.length} games selected` }}</strong>
                <small v-if="picked.length === 1">Ctrl+click to add more games</small>
                <small v-else>{{ picked.map((p) => p.name).join(', ') }}</small>
              </span>
              <button class="icon-btn" aria-label="Clear selection" @click="picked = []">
                <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
                  <path d="M5 5l10 10M15 5L5 15" />
                </svg>
              </button>
            </div>

            <div class="dock-controls">
              <label class="minutes">
                <span class="sr">Minutes to run</span>
                <input type="number" min="1" max="240" step="1" v-model.number="minutes" />
                <span class="unit">min</span>
              </label>
              <div class="chips" role="group" aria-label="Quick durations">
                <button
                  v-for="m in PRESETS"
                  :key="m"
                  class="chip"
                  :class="{ on: Number(minutes) === m }"
                  @click="minutes = m"
                >
                  {{ m }}
                </button>
              </div>
              <button class="btn primary start" :disabled="busy" @click="startQuests">
                {{ startLabel }}
              </button>
            </div>
          </div>
        </Transition>
      </section>

      <!-- Stage -->
      <aside class="stage" aria-label="Quest progress">
        <div class="orb-wrap" :class="{ live: !!focus }">
          <svg
            class="orb"
            viewBox="0 0 240 240"
            role="img"
            :aria-label="focus ? `${focus.name}: ${fmt(remaining)} remaining` : 'No quest running'"
          >
            <defs>
              <linearGradient id="arc" x1="0" y1="0" x2="1" y2="1">
                <stop class="arc-a" offset="0" />
                <stop class="arc-b" offset="1" />
              </linearGradient>
              <radialGradient id="core" cx="0.35" cy="0.3" r="0.85">
                <stop class="core-a" offset="0" stop-opacity="0.5" />
                <stop class="core-b" offset="0.28" stop-opacity="0.85" />
                <stop class="core-c" offset="0.62" stop-opacity="0.92" />
                <stop class="core-d" offset="1" stop-opacity="0.96" />
              </radialGradient>
              <radialGradient id="shine" cx="0.5" cy="0.5" r="0.5">
                <stop offset="0" stop-color="#ffffff" stop-opacity="0.5" />
                <stop offset="1" stop-color="#ffffff" stop-opacity="0" />
              </radialGradient>
              <mask id="dial" maskUnits="userSpaceOnUse" x="0" y="0" width="240" height="240">
                <circle
                  cx="120"
                  cy="120"
                  r="108"
                  fill="none"
                  stroke="#ffffff"
                  stroke-width="14"
                  :stroke-dasharray="DIAL_DASH"
                />
              </mask>
            </defs>

            <g mask="url(#dial)">
              <circle class="orb-track" cx="120" cy="120" r="108" />
              <circle
                class="orb-arc"
                cx="120"
                cy="120"
                r="108"
                stroke="url(#arc)"
                transform="rotate(-90 120 120)"
                :stroke-dasharray="ORB_C"
                :stroke-dashoffset="orbOffset"
                :opacity="focus ? 1 : 0"
              />
            </g>
            <circle class="orb-core" cx="120" cy="120" r="88" fill="url(#core)" />
            <ellipse class="orb-shine" cx="92" cy="82" rx="42" ry="24" fill="url(#shine)" transform="rotate(-28 92 82)" />

            <text class="orb-time" :class="{ idle: !focus }" x="120" :y="focus ? 128 : 130" text-anchor="middle">
              {{ focus ? fmt(remaining) : 'Ready' }}
            </text>
            <text v-if="focus" class="orb-sub" x="120" y="152" text-anchor="middle">
              {{ focus.done ? 'finishing up' : 'remaining' }}
            </text>
          </svg>
        </div>

        <div v-if="focus" class="focus-info">
          <h2>{{ focus.name }}</h2>
          <p>{{ fmt(focus.elapsed_secs) }} of {{ fmt(focus.total_secs) }}, {{ percent(focus) }}% done</p>
          <p class="eta">Finishes at {{ finishAt }}</p>
          <div class="focus-actions">
            <button class="btn ghost" @click="stopQuest(focus.id)">Stop quest</button>
            <button v-if="quests.length > 1" class="btn ghost" @click="stopAll">Stop all</button>
          </div>
        </div>
        <div v-else class="focus-info">
          <h2>No quest running</h2>
          <p>Pick a game, set the minutes, then start. The dial fills as time passes.</p>
        </div>

        <ul v-if="quests.length > 1" class="others">
          <li v-for="q in quests" :key="q.id">
            <button class="other" :class="{ on: focus && focus.id === q.id }" @click="focusId = q.id">
              <span class="other-name">{{ q.name }}</span>
              <span class="other-time">{{ fmt(timeLeft(q)) }}</span>
              <span class="other-bar"><span :style="{ width: percent(q) + '%' }"></span></span>
            </button>
          </li>
          <li class="others-note">All finish by {{ allDoneAt }}</li>
        </ul>

        <div v-if="finished.length" class="completed">
          <div class="completed-head">
            <h3>Completed</h3>
            <button class="link" @click="finished = []">Clear</button>
          </div>
          <ul>
            <li v-for="f in finished" :key="f.key">
              <span class="tick" aria-hidden="true">✓</span>
              <span class="done-name">{{ f.name }}</span>
              <time>{{ clock(f.at) }}</time>
            </li>
          </ul>
        </div>

        <dl class="stats">
          <div><dt>Finished this session</dt><dd>{{ finished.length }}</dd></div>
          <div><dt>Finished all time</dt><dd>{{ stats.quests }}</dd></div>
          <div><dt>Total run time</dt><dd>{{ fmtHours(stats.minutes) }}</dd></div>
        </dl>
        <button v-if="stats.quests" class="link reset" @click="resetStats">Reset stats</button>
      </aside>
    </div>
  </div>
</template>

<style>
:root {
  --bg: #000000;
  --panel: #0a0a0a;
  --raised: #141414;
  --raised-2: #1d1d1d;
  --line: #2a2a2a;
  --text: #f2f2f2;
  --muted: #9a9a9a;
  --dim: #666666;
  --ok: #3ddc97;
  --warn: #ffb547;

  --accent: #ff2b4a;
  --accent-2: #ff7a59;
  --accent-rgb: 255, 43, 74;
  --orb-deep: #2b040b;
  color-scheme: dark;
}

:root[data-accent='blood'] {
  --accent: #d4142a;
  --accent-2: #ff5a5a;
  --accent-rgb: 212, 20, 42;
  --orb-deep: #240308;
}

:root[data-accent='ember'] {
  --accent: #ff5a1f;
  --accent-2: #ffb02e;
  --accent-rgb: 255, 90, 31;
  --orb-deep: #2e0f03;
}

:root[data-accent='rose'] {
  --accent: #ff4d8d;
  --accent-2: #ff9a6b;
  --accent-rgb: 255, 77, 141;
  --orb-deep: #2b0616;
}

html,
body,
#app {
  height: 100%;
  margin: 0;
}

body {
  background: var(--bg);
  color: var(--text);
  font-family: 'Segoe UI Variable Text', 'Segoe UI', system-ui, sans-serif;
  font-size: 14px;
  line-height: 1.45;
}

*,
*::before,
*::after {
  box-sizing: border-box;
}

* {
  scrollbar-width: thin;
  scrollbar-color: var(--line) transparent;
}

button,
input {
  font: inherit;
  color: inherit;
}

button:focus-visible,
input:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: 2px;
}

.sr {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
  white-space: nowrap;
}

kbd {
  padding: 1px 6px;
  background: var(--raised-2);
  border: 1px solid var(--line);
  border-radius: 4px;
  font: inherit;
  font-size: 12px;
}

/* Frame -------------------------------------------------------------- */
.app {
  display: flex;
  flex-direction: column;
  height: 100%;
}

.bar {
  position: relative;
  z-index: 20;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  padding: 14px 24px;
  flex-wrap: wrap;
}

.brand {
  display: flex;
  align-items: center;
  gap: 11px;
}

.brand h1 {
  margin: 0;
  font-family: 'Segoe UI Variable Display', 'Segoe UI', system-ui, sans-serif;
  font-size: 19px;
  font-weight: 700;
  letter-spacing: -0.02em;
}

.mark {
  width: 20px;
  height: 20px;
  border-radius: 50%;
  background: radial-gradient(circle at 32% 28%, #ffffff 0, var(--accent-2) 26%, var(--accent) 62%, var(--orb-deep) 100%);
  box-shadow: 0 0 14px rgba(var(--accent-rgb), 0.6);
}

.actions {
  display: flex;
  align-items: center;
  gap: 12px;
}

.status {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  color: var(--muted);
}

.status.on {
  color: var(--text);
}

.dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--warn);
}

.status.on .dot {
  background: var(--ok);
  box-shadow: 0 0 8px rgba(61, 220, 151, 0.6);
}

.banner {
  display: flex;
  align-items: center;
  gap: 12px;
  margin: 0 24px 12px;
  padding: 10px 14px;
  background: rgba(var(--accent-rgb), 0.1);
  border: 1px solid rgba(var(--accent-rgb), 0.45);
  border-radius: 8px;
}

.banner span {
  flex: 1;
}

.body {
  flex: 1;
  min-height: 0;
  display: grid;
  grid-template-columns: minmax(0, 1fr) 330px;
  border-top: 1px solid var(--line);
}

/* Settings ----------------------------------------------------------- */
.scrim {
  position: fixed;
  inset: 0;
  z-index: 10;
}

.settings {
  position: absolute;
  top: calc(100% - 4px);
  right: 24px;
  z-index: 30;
  width: 300px;
  padding: 8px 16px 14px;
  background: var(--raised);
  border: 1px solid var(--line);
  border-radius: 12px;
  box-shadow: 0 20px 50px -10px rgba(0, 0, 0, 0.9), 0 0 0 1px rgba(var(--accent-rgb), 0.08);
}

.set-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 10px 0;
  border-bottom: 1px solid var(--line);
}

.set-row strong {
  display: block;
  font-weight: 600;
}

.set-row small {
  display: block;
  color: var(--muted);
  font-size: 12px;
}

.switch {
  position: relative;
  flex: none;
  width: 38px;
  height: 22px;
  padding: 0;
  background: var(--raised-2);
  border: 1px solid var(--line);
  border-radius: 999px;
  cursor: pointer;
  transition: background 0.15s ease, border-color 0.15s ease;
}

.switch span {
  position: absolute;
  top: 2px;
  left: 2px;
  width: 16px;
  height: 16px;
  border-radius: 50%;
  background: var(--muted);
  transition: transform 0.15s ease, background 0.15s ease;
}

.switch.on {
  background: rgba(var(--accent-rgb), 0.25);
  border-color: var(--accent);
}

.switch.on span {
  transform: translateX(16px);
  background: var(--accent);
}

.swatches {
  display: flex;
  gap: 8px;
}

.swatch {
  width: 20px;
  height: 20px;
  padding: 0;
  border: 2px solid transparent;
  border-radius: 50%;
  cursor: pointer;
}

.swatch.on {
  border-color: var(--text);
}

.keys {
  margin: 12px 0 0;
  display: flex;
  flex-direction: column;
  gap: 6px;
  color: var(--muted);
  font-size: 12px;
}

.keys div {
  display: flex;
  justify-content: space-between;
  gap: 12px;
}

.keys dt,
.keys dd {
  margin: 0;
}

.pop-enter-active,
.pop-leave-active {
  transition: opacity 0.14s ease, transform 0.14s ease;
}

.pop-enter-from,
.pop-leave-to {
  opacity: 0;
  transform: translateY(-6px);
}

/* Library ------------------------------------------------------------ */
.library {
  display: flex;
  flex-direction: column;
  min-height: 0;
  min-width: 0;
}

.search {
  position: relative;
  margin: 16px 20px 10px;
}

.search input {
  width: 100%;
  padding: 10px 84px 10px 38px;
  background: var(--raised);
  border: 1px solid var(--line);
  border-radius: 10px;
  transition: border-color 0.15s ease, box-shadow 0.15s ease;
}

.search input:focus {
  outline: none;
  border-color: var(--accent);
  box-shadow: 0 0 0 3px rgba(var(--accent-rgb), 0.18);
}

.search input::placeholder {
  color: var(--muted);
}

.mag {
  position: absolute;
  left: 12px;
  top: 50%;
  width: 16px;
  height: 16px;
  transform: translateY(-50%);
  color: var(--muted);
  pointer-events: none;
}

.found {
  position: absolute;
  right: 12px;
  top: 50%;
  transform: translateY(-50%);
  color: var(--muted);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  pointer-events: none;
}

.tabs {
  display: flex;
  gap: 4px;
  margin: 0 20px 8px;
  border-bottom: 1px solid var(--line);
}

.tab {
  position: relative;
  padding: 8px 12px;
  background: none;
  border: 0;
  color: var(--muted);
  cursor: pointer;
}

.tab:hover {
  color: var(--text);
}

.tab.on {
  color: var(--text);
  font-weight: 600;
}

.tab.on::after {
  content: '';
  position: absolute;
  left: 8px;
  right: 8px;
  bottom: -1px;
  height: 2px;
  border-radius: 2px;
  background: var(--accent);
  box-shadow: 0 0 10px rgba(var(--accent-rgb), 0.7);
}

.count {
  margin-left: 6px;
  padding: 0 6px;
  border-radius: 999px;
  background: var(--raised-2);
  color: var(--muted);
  font-size: 11px;
  font-weight: 500;
  font-variant-numeric: tabular-nums;
}

.tab.on .count {
  background: rgba(var(--accent-rgb), 0.2);
  color: var(--accent);
}

.state {
  margin: 0;
  padding: 40px 20px;
  text-align: center;
  color: var(--muted);
}

.state p {
  margin: 0 0 12px;
}

.skeletons {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 4px 20px;
}

.sk {
  height: 44px;
  border-radius: 10px;
  background: linear-gradient(90deg, var(--raised) 0%, var(--raised-2) 50%, var(--raised) 100%);
  background-size: 200% 100%;
  animation: shimmer 1.4s linear infinite;
}

@keyframes shimmer {
  from {
    background-position: 200% 0;
  }
  to {
    background-position: -200% 0;
  }
}

.games {
  flex: 1;
  min-height: 0;
  margin: 0;
  padding: 0 10px 10px;
  list-style: none;
  overflow-y: auto;
}

.row {
  display: flex;
  align-items: center;
  border-radius: 10px;
}

.row:hover {
  background: var(--raised);
}

.row.active {
  background: rgba(var(--accent-rgb), 0.12);
  box-shadow: inset 2px 0 0 var(--accent);
}

.row.cursor {
  box-shadow: inset 0 0 0 1px rgba(var(--accent-rgb), 0.5);
}

.row.active.cursor {
  box-shadow: inset 2px 0 0 var(--accent), inset 0 0 0 1px rgba(var(--accent-rgb), 0.5);
}

.game {
  flex: 1;
  min-width: 0;
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 7px 8px 7px 12px;
  background: transparent;
  border: 0;
  border-radius: 10px;
  text-align: left;
  cursor: pointer;
}

.game-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.badge {
  flex: none;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  color: var(--accent);
  font-size: 12px;
}

.badge i {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--accent);
  animation: pulse 1.6s ease-in-out infinite;
}

@keyframes pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.25;
  }
}

.star {
  flex: none;
  display: grid;
  place-items: center;
  width: 32px;
  height: 32px;
  margin-right: 6px;
  padding: 0;
  background: transparent;
  border: 0;
  border-radius: 8px;
  color: var(--dim);
  cursor: pointer;
  opacity: 0;
  transition: opacity 0.12s ease, color 0.12s ease;
}

.row:hover .star,
.row:focus-within .star,
.star.on {
  opacity: 1;
}

.star:hover {
  background: var(--raised-2);
  color: var(--text);
}

.star.on {
  color: var(--accent);
}

.star svg {
  width: 16px;
  height: 16px;
  fill: none;
  stroke: currentColor;
  stroke-width: 1.6;
  stroke-linejoin: round;
}

.star.on svg {
  fill: currentColor;
}

.more {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 6px;
  padding: 14px 12px 6px;
  color: var(--muted);
  font-size: 12px;
}

.tile {
  flex: none;
  display: grid;
  place-items: center;
  width: 32px;
  height: 32px;
  overflow: hidden;
  border-radius: 8px;
  background: var(--raised-2);
  color: rgba(255, 255, 255, 0.85);
  font-size: 12px;
  font-weight: 650;
}

.tile img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}

.compact .game {
  padding-top: 3px;
  padding-bottom: 3px;
}

.compact .row .tile {
  width: 24px;
  height: 24px;
  border-radius: 6px;
  font-size: 10px;
}

.compact .star {
  width: 26px;
  height: 26px;
}

/* Launch dock -------------------------------------------------------- */
.dock {
  display: flex;
  flex-direction: column;
  gap: 12px;
  margin: 0 14px 14px;
  padding: 14px;
  background: var(--raised);
  border: 1px solid var(--line);
  border-radius: 14px;
  box-shadow: 0 -12px 40px -18px rgba(var(--accent-rgb), 0.55);
}

.dock-game {
  display: flex;
  align-items: center;
  gap: 12px;
}

.stack {
  display: flex;
  flex: none;
}

.stack .tile {
  margin-left: -10px;
  border: 2px solid var(--raised);
}

.stack .tile:first-child {
  margin-left: 0;
}

.stack .plus {
  background: var(--raised-2);
  color: var(--muted);
}

.dock-text {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
}

.dock-text strong,
.dock-text small {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.dock-text strong {
  font-weight: 620;
}

.dock-text small {
  color: var(--muted);
  font-size: 12px;
}

.icon-btn {
  display: grid;
  place-items: center;
  flex: none;
  width: 32px;
  height: 32px;
  padding: 0;
  background: transparent;
  border: 0;
  border-radius: 8px;
  color: var(--muted);
  cursor: pointer;
}

.icon-btn:hover,
.icon-btn.lit {
  background: var(--raised-2);
  color: var(--text);
}

.icon-btn svg {
  width: 16px;
  height: 16px;
}

.dock-controls {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}

.minutes {
  display: inline-flex;
  align-items: center;
  gap: 8px;
}

.minutes input {
  width: 68px;
  padding: 8px 10px;
  background: var(--bg);
  border: 1px solid var(--line);
  border-radius: 8px;
  font-variant-numeric: tabular-nums;
}

.minutes input:focus {
  outline: none;
  border-color: var(--accent);
}

.unit {
  color: var(--muted);
}

.chips {
  display: flex;
  gap: 6px;
}

.chip {
  padding: 7px 11px;
  background: transparent;
  border: 1px solid var(--line);
  border-radius: 8px;
  color: var(--muted);
  cursor: pointer;
  font-variant-numeric: tabular-nums;
}

.chip:hover {
  color: var(--text);
}

.chip.on {
  border-color: var(--accent);
  background: rgba(var(--accent-rgb), 0.12);
  color: var(--accent);
}

.start {
  margin-left: auto;
}

.rise-enter-active,
.rise-leave-active {
  transition: transform 0.16s ease, opacity 0.16s ease;
}

.rise-enter-from,
.rise-leave-to {
  transform: translateY(12px);
  opacity: 0;
}

/* Stage -------------------------------------------------------------- */
.stage {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 18px;
  min-height: 0;
  padding: 28px 24px 20px;
  overflow-y: auto;
  background: radial-gradient(360px 300px at 50% 130px, rgba(var(--accent-rgb), 0.1), transparent 70%), var(--panel);
  border-left: 1px solid var(--line);
}

.orb-wrap {
  flex: none;
  width: 216px;
  height: 216px;
}

.orb {
  width: 100%;
  height: 100%;
  overflow: visible;
  transition: filter 0.4s ease;
}

.orb-wrap.live .orb {
  filter: drop-shadow(0 0 22px rgba(var(--accent-rgb), 0.5));
}

.arc-a {
  stop-color: var(--accent-2);
}

.arc-b {
  stop-color: var(--accent);
}

.core-a {
  stop-color: #ffffff;
}

.core-b {
  stop-color: var(--accent-2);
}

.core-c {
  stop-color: var(--accent);
}

.core-d {
  stop-color: var(--orb-deep);
}

.orb-track {
  fill: none;
  stroke: var(--raised-2);
  stroke-width: 9;
}

.orb-arc {
  fill: none;
  stroke-width: 9;
  transition: stroke-dashoffset 1s linear, opacity 0.3s ease;
}

.orb-core {
  opacity: 0.4;
  transition: opacity 0.4s ease;
}

.orb-wrap.live .orb-core {
  opacity: 1;
  transform-box: fill-box;
  transform-origin: center;
  animation: breathe 5s ease-in-out infinite;
}

.orb-shine {
  pointer-events: none;
}

.orb-time {
  fill: #ffffff;
  font-size: 42px;
  font-weight: 300;
  font-variant-numeric: tabular-nums;
}

.orb-time.idle {
  fill: var(--muted);
  font-size: 26px;
  font-weight: 400;
}

.orb-sub {
  fill: rgba(255, 255, 255, 0.75);
  font-size: 13px;
}

@keyframes breathe {
  0%,
  100% {
    transform: scale(1);
  }
  50% {
    transform: scale(1.025);
  }
}

.focus-info {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  max-width: 100%;
  text-align: center;
}

.focus-info h2 {
  margin: 0;
  font-size: 16px;
  font-weight: 650;
  overflow-wrap: anywhere;
}

.focus-info p {
  margin: 0;
  color: var(--muted);
  font-variant-numeric: tabular-nums;
}

.focus-info .eta {
  margin-bottom: 10px;
  color: var(--accent);
}

.focus-actions {
  display: flex;
  gap: 8px;
}

.others {
  width: 100%;
  margin: 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.other {
  display: grid;
  grid-template-columns: 1fr auto;
  gap: 4px 10px;
  width: 100%;
  padding: 8px 10px;
  background: transparent;
  border: 1px solid var(--line);
  border-radius: 8px;
  text-align: left;
  cursor: pointer;
}

.other:hover {
  background: var(--raised);
}

.other.on {
  border-color: var(--accent);
}

.other-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.other-time {
  color: var(--muted);
  font-variant-numeric: tabular-nums;
}

.other-bar {
  grid-column: 1 / -1;
  height: 3px;
  overflow: hidden;
  border-radius: 999px;
  background: var(--raised-2);
}

.other-bar span {
  display: block;
  height: 100%;
  background: var(--accent);
  transition: width 1s linear;
}

.others-note {
  color: var(--muted);
  font-size: 12px;
  text-align: center;
}

.completed {
  width: 100%;
  padding-top: 14px;
  border-top: 1px solid var(--line);
}

.completed-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 6px;
}

.completed h3 {
  margin: 0;
  color: var(--muted);
  font-size: 13px;
  font-weight: 500;
}

.completed ul {
  margin: 0;
  padding: 0;
  list-style: none;
}

.completed li {
  display: flex;
  align-items: baseline;
  gap: 8px;
  padding: 3px 0;
}

.tick {
  color: var(--ok);
}

.done-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.completed time {
  color: var(--dim);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
}

.stats {
  width: 100%;
  margin: auto 0 0;
  padding-top: 14px;
  border-top: 1px solid var(--line);
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.stats div {
  display: flex;
  justify-content: space-between;
  gap: 12px;
}

.stats dt {
  color: var(--muted);
}

.stats dd {
  margin: 0;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
}

.reset {
  align-self: flex-end;
  margin-top: -10px;
  color: var(--dim);
  font-size: 12px;
}

/* Buttons ------------------------------------------------------------ */
.btn {
  padding: 8px 14px;
  background: transparent;
  border: 1px solid var(--line);
  border-radius: 8px;
  cursor: pointer;
  transition: background 0.12s ease, border-color 0.12s ease, filter 0.12s ease;
}

.btn:hover {
  background: var(--raised-2);
}

.btn:disabled {
  opacity: 0.5;
  cursor: default;
}

.btn.primary {
  background: var(--accent);
  border-color: var(--accent);
  color: #ffffff;
  font-weight: 650;
  box-shadow: 0 6px 20px -6px rgba(var(--accent-rgb), 0.8);
}

.btn.primary:hover:not(:disabled) {
  background: var(--accent);
  filter: brightness(1.12);
}

.btn.warn {
  border-color: var(--accent);
  color: var(--accent);
}

.link {
  padding: 0;
  background: none;
  border: 0;
  color: var(--accent);
  cursor: pointer;
}

.link:hover {
  text-decoration: underline;
}

/* Small screens ------------------------------------------------------ */
@media (max-width: 760px) {
  .body {
    grid-template-columns: 1fr;
    overflow-y: auto;
  }
  .library {
    max-height: 520px;
  }
  .stage {
    border-left: 0;
    border-top: 1px solid var(--line);
  }
}

@media (prefers-reduced-motion: reduce) {
  .orb-wrap.live .orb-core,
  .badge i,
  .sk {
    animation: none;
  }
  .orb-arc,
  .other-bar span,
  .rise-enter-active,
  .rise-leave-active,
  .pop-enter-active,
  .pop-leave-active {
    transition: none;
  }
}
</style>