// ── Auth state ──
let currentUser = null;
let devEnabled = false;   // DEV mode flag from /api/config
let devUser = null;       // Slack ID being previewed in DEV mode
let devUsers = [];        // [{slack_id, display_name}] for the DEV autocomplete

async function checkAuth() {
  try {
    const r = await fetch('/api/auth/me');
    if (r.ok) {
      currentUser = await r.json();
      renderUser();
      loadMyProjects();
    } else {
      currentUser = null;
      renderUser();
    }
  } catch { currentUser = null; renderUser(); }
}

function renderUser() {
  const area = document.getElementById('user-area');
  const btn = document.getElementById('login-btn');
  if (currentUser) {
    btn.style.display = 'none';
    area.innerHTML = `
      <div class="user-badge">
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/></svg>
        ${escHtml(currentUser.display_name || currentUser.sub)}
        <button class="logout-btn" onclick="logout()">Logout</button>
      </div>`;
  } else {
    btn.style.display = '';
    area.innerHTML = '';
  }
}

async function logout() {
  await fetch('/api/auth/logout');
  currentUser = null;
  renderUser();
  document.getElementById('projects-content').innerHTML = '<div class="island-empty">Log in to see your projects review status.</div>';
}

// ── Login button ──
document.getElementById('login-btn').addEventListener('click', async (e) => {
  e.preventDefault();
  try {
    const r = await fetch('/api/auth/login');
    const d = await r.json();
    if (d.url) window.location.href = d.url;
  } catch (err) { console.error('Login failed', err); }
});

// ── My projects ──
async function loadMyProjects() {
  const island = document.getElementById('island');
  const content = document.getElementById('projects-content');

  if (!currentUser && !devUser) {
    content.innerHTML = '<div class="island-empty">Log in to see your projects review status.</div>';
    return;
  }

  island.classList.add('open');
  const titleEl = document.getElementById('island-title');
  if (titleEl) titleEl.textContent = devUser ? `Projects · ${devUser}` : 'My Submitted Projects';
  const who = devUser ? ` for ${escHtml(devUser)}` : '';
  content.innerHTML = `<div class="island-loading">Loading projects${who}...</div>`;

  try {
    const url = devUser
      ? `/api/my/projects?user=${encodeURIComponent(devUser)}`
      : '/api/my/projects';
    const r = await fetch(url);
    if (!r.ok) throw new Error(await r.text());
    const projects = await r.json();

    if (!projects.length) {
      content.innerHTML = '<div class="island-empty">No projects found</div>';
      return;
    }

      content.innerHTML = projects.map(p => {
        const isQueue = p.source === 'queue';
        const isPending = p.status === 'pending';
        const meta = p.projectType ? p.projectType.replace(/_/g, ' ') : '';

        let mainBadges;
        if (isPending) {
          const fraudStatus = getFraudStatus(p);
          const reviewStatus = getReviewStatus(p);

          const fraudClass = getBadgeColorClass(fraudStatus);
          const reviewClass = getBadgeColorClass(reviewStatus);

          const queuePos = (p.queuePosition != null && p.queuePosition > 0)
            ? `<span class="badge badge-queue-pos">#${p.queuePosition} in queue</span>` : '';
          const claimed = (isQueue && p.claimed)
            ? `<span class="badge badge-claimed">Being reviewed</span>` : '';

          mainBadges = `
            <span class="badge ${fraudClass}">Fraud ${fraudStatus}</span>
            <span class="badge ${reviewClass}">Review ${reviewStatus}</span>
            ${queuePos}
            ${claimed}`;
        } else {
          const badgeClass  = statusBadgeClass(p.status);
          const badgeLabel  = statusLabel(p.status, p.reviewStage);
          mainBadges = `<span class="badge ${badgeClass}">${badgeLabel}</span>`;
        }

        return `<div class="project-item">
          <div class="project-row">
            <div class="project-info">
              <div class="project-title">${escHtml(p.projectTitle || '(untitled)')}</div>
              <div class="project-meta">${escHtml(meta)}</div>
            </div>
            <div class="project-status">
              ${mainBadges}
            </div>
          </div>
          ${renderFeedback(p)}
          ${renderTimeline(p)}
        </div>`;
      }).join('');
  } catch (e) {
    content.innerHTML = `<div class="island-error">Failed to load: ${escHtml(e.message)}</div>`;
  }
}

function getFraudStatus(p) {
  if (p.source === 'fraud_rejected' || p.reviewStage === 'Fraud Rejected') {
    return 'Rejected';
  }
  if (p.source === 'queue') {
    if (p.joeFraudPassed === true) return 'Approved';
    return 'Pending';
  }
  if (p.source === 'past') {
    if (p.approvalStatus === 'approved' || p.approvalStatus === 'finalized') {
      return 'Approved';
    }
    if (p.approvalStatus === 'rejected') {
      if (p.reviewPassed === true) return 'Rejected';
      return 'Approved';
    }
    if (p.approvalStatus === 'pending') {
      return 'Pending';
    }
  }
  return 'Pending';
}

function getReviewStatus(p) {
  if (p.status === 'approved' || p.status === 'finalized') {
    return 'Approved';
  }
  if (p.source === 'queue') {
    return 'Pending';
  }
  if (p.source === 'past') {
    if (p.reviewPassed === true) return 'Approved';
    if (p.reviewPassed === false) return 'Rejected';
    if (p.reviewPassed === null || p.reviewPassed === undefined) {
      if (p.approvalStatus === 'approved') return 'Approved';
      if (p.approvalStatus === 'rejected') return 'Rejected';
    }
  }
  return 'Pending';
}

function getBadgeColorClass(status) {
  switch (status) {
    case 'Approved': return 'badge-status-approved';
    case 'Rejected': return 'badge-status-rejected';
    case 'Pending':  return 'badge-status-pending';
    default:         return 'badge-unsubmitted';
  }
}

function statusBadgeClass(status) {
  switch (status) {
    case 'pending':       return 'badge-pending';
    case 'approved':      return 'badge-approved';
    case 'rejected':      return 'badge-rejected';
    case 'in_review':     return 'badge-in-review';
    case 'finalized':     return 'badge-finalized';
    case 'unsubmitted':   return 'badge-unsubmitted';
    default:              return 'badge-unsubmitted';
  }
}

function statusLabel(status, reviewStage) {
  if (reviewStage === 'Fraud Rejected') return 'Fraud Rejected';
  if (reviewStage === 'Not Started')    return 'In Queue';
  if (reviewStage === 'Fraud Review')   return 'Fraud Review';
  if (reviewStage === 'Normal Review')  return 'In Review';
  switch (status) {
    case 'pending':     return 'Pending';
    case 'approved':    return 'Approved';
    case 'rejected':    return 'Rejected';
    case 'in_review':   return 'In Review';
    case 'finalized':   return 'Finalized';
    case 'unsubmitted': return 'Unsubmitted';
    default:            return status ? status.replace(/_/g, ' ') : 'Unknown';
  }
}

function escHtml(s) {
  const d = document.createElement('div');
  d.textContent = s || '';
  return d.innerHTML;
}

// ── Reviewer feedback + timeline ──
function renderFeedback(p) {
  const fb = (p.latestFeedback || '').trim();
  if (!fb) return '';
  return `<div class="project-feedback">
    <span class="feedback-label">Reviewer feedback</span>
    <span class="feedback-text">${escHtml(fb)}</span>
  </div>`;
}

function renderTimeline(p) {
  const tl = Array.isArray(p.timeline) ? p.timeline : [];
  // A lone "submitted" entry isn't an interesting history; only show when there's
  // at least one review or resubmission to look back on.
  if (tl.length < 2) return '';

  const rows = tl.map(e => {
    const type = e.type || '';
    const hours = (e.approvedHours != null) ? e.approvedHours
                : (e.submittedHours != null) ? e.submittedHours
                : (e.hours != null) ? e.hours : null;
    const bits = [];
    if (hours != null) bits.push(`${hours}h`);
    if (e.timestamp) bits.push(fmtDate(e.timestamp));
    const efb = (e.userFeedback || '').trim();
    return `<div class="tl-event tl-${escHtml(type)}">
      <div class="tl-head">
        <span class="tl-type">${escHtml(tlLabel(type))}</span>
        <span class="tl-meta">${bits.join(' · ')}</span>
      </div>
      ${efb ? `<div class="tl-feedback">${escHtml(efb)}</div>` : ''}
    </div>`;
  }).join('');

  return `<button class="timeline-toggle" data-show="Show timeline (${tl.length})" data-hide="Hide timeline">Show timeline (${tl.length})</button>
    <div class="timeline" hidden>${rows}</div>`;
}

function tlLabel(type) {
  switch (type) {
    case 'submitted':   return 'Submitted';
    case 'resubmitted': return 'Resubmitted';
    case 'approved':    return 'Approved';
    case 'rejected':    return 'Rejected';
    default:            return type ? type.replace(/_/g, ' ') : 'Event';
  }
}

function fmtDate(ts) {
  const d = new Date(ts);
  if (isNaN(d)) return '';
  return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' });
}

// ── Event blobs ──
async function loadEvents() {
  const skel = document.getElementById('events-skel');
  const cont = document.getElementById('events-content');
  try {
    const r = await fetch('/api/events');
    if (!r.ok) throw new Error(await r.text());
    const data = await r.json();
    if (data.error) throw new Error(data.error);
    if (!data.events || !data.events.length) {
      skel.style.display = 'none';
      cont.style.display = '';
      cont.innerHTML = '<div class="island-empty">No approved projects found.</div>';
      return;
    }
    skel.style.display = 'none';
    cont.style.display = '';
    cont.innerHTML = '<div class="events-grid">' +
      data.events.map(e => `<div class="event-blob">
        <div class="event-blob-name">${escHtml(e.title)}</div>
        <div class="event-stat">
          <div class="event-stat-num">${e.approvedProjects}</div>
          <div class="event-stat-label">projects</div>
        </div>
        <div class="event-stat">
          <div class="event-stat-num event-stat-num-sm">${Math.round(e.approvedHours)}</div>
          <div class="event-stat-label">hours</div>
        </div>
      </div>`).join('') +
      '</div>';
  } catch (e) {
    console.error('Failed to load events:', e);
    skel.style.display = '';
    cont.style.display = 'none';
  }
}

// ── Pipeline chart ──
async function loadStats() {
  const skel = document.getElementById('skel');
  const chart = document.getElementById('chart');
  try {
    const r = await fetch('/api/stats');
    if (!r.ok) throw new Error(await r.text());
    const d = await r.json();
    if (d.error) throw new Error(d.error);

    const total = d.total_pending;
    const jf = d.just_fraud_review_pending;
    const b = Math.max(0, d.fraud_review_pending - d.just_fraud_review_pending);
    const jr = d.just_normal_review_pending;

    skel.style.display = 'none';
    chart.style.display = '';

    document.getElementById('total-num').textContent = total;

    const pct = v => total > 0 ? (v / total) * 100 : 0;
    const segs = [
      { id: 'seg-jf', v: jf },
      { id: 'seg-b',  v: b },
      { id: 'seg-jr', v: jr },
    ];
    for (const s of segs) {
      const el = document.getElementById(s.id);
      el.style.width = pct(s.v) + '%';
      el.querySelector('span').textContent = s.v > 0 ? s.v : '';
    }
  } catch (e) {
    console.error('Failed to load stats:', e);
    skel.style.display = '';
    chart.style.display = 'none';
  }
}

// ── Timeline expand/collapse (event delegation; container persists) ──
document.getElementById('projects-content').addEventListener('click', (e) => {
  const btn = e.target.closest('.timeline-toggle');
  if (!btn) return;
  const tl = btn.nextElementSibling;
  if (!tl || !tl.classList.contains('timeline')) return;
  if (tl.hasAttribute('hidden')) {
    tl.removeAttribute('hidden');
    btn.textContent = btn.dataset.hide;
  } else {
    tl.setAttribute('hidden', '');
    btn.textContent = btn.dataset.show;
  }
});

// ── DEV user-override box ──
async function initDevBox() {
  try {
    const r = await fetch('/api/config');
    const d = await r.json();
    devEnabled = !!d.dev;
  } catch { devEnabled = false; }
  if (!devEnabled) return;

  const box = document.getElementById('dev-box');
  const input = document.getElementById('dev-user');
  box.style.display = '';

  // Populate the autocomplete with every user in the pipeline. Each option's
  // value is "Display Name · <slackId>" so typing a name filters the list;
  // apply() pulls the trailing Slack ID back out (raw IDs still work too).
  loadDevUsers();

  const apply = () => {
    devUser = resolveDevUser(input.value);
    loadMyProjects();
  };
  document.getElementById('dev-go').addEventListener('click', apply);
  input.addEventListener('keydown', (e) => { if (e.key === 'Enter') apply(); });
  document.getElementById('dev-clear').addEventListener('click', () => {
    input.value = '';
    devUser = null;
    loadMyProjects();
  });

  // Deep-link preview: ?user=<slackId> auto-loads that user (DEV only).
  const preset = new URLSearchParams(location.search).get('user');
  if (preset) {
    input.value = preset;
    apply();
  }
}

async function loadDevUsers() {
  try {
    const r = await fetch('/api/dev/users');
    if (!r.ok) return;
    devUsers = await r.json();
  } catch { return; }
  const list = document.getElementById('dev-user-list');
  if (!list) return;
  list.innerHTML = devUsers.map(u =>
    `<option value="${escHtml(u.display_name)} · ${escHtml(u.slack_id)}"></option>`
  ).join('');
}

// Turn the DEV box input into a Slack ID: accept "Name · U123", a raw "U123",
// or a bare display name (resolved against the loaded user list).
function resolveDevUser(raw) {
  const v = (raw || '').trim();
  if (!v) return null;
  const sep = v.lastIndexOf('·');
  if (sep !== -1) return v.slice(sep + 1).trim() || null;
  const byName = devUsers.find(u => u.display_name === v);
  return byName ? byName.slack_id : v;
}

loadStats();
setInterval(loadStats, 30000);
loadEvents();
setInterval(loadEvents, 30000);
checkAuth();
initDevBox();
