// ── Global loading bar ──
// Wrap fetch so any in-flight request drives the top progress bar. A short
// delay keeps quick requests (config, stats) from flashing it; only slower
// ones (dev users, event stats, per-project timelines) actually show.
(function () {
  const nativeFetch = window.fetch.bind(window);
  let inflight = 0;
  let showTimer = null;
  const bar = () => document.getElementById('loadbar');
  function show() { const b = bar(); if (b) b.classList.add('active'); }
  function hide() { const b = bar(); if (b) b.classList.remove('active'); }
  window.fetch = function (...args) {
    inflight++;
    if (!showTimer) showTimer = setTimeout(() => { showTimer = null; show(); }, 150);
    return nativeFetch(...args).finally(() => {
      inflight = Math.max(0, inflight - 1);
      if (inflight === 0) {
        if (showTimer) { clearTimeout(showTimer); showTimer = null; }
        hide();
      }
    });
  };
})();

// ── Auth state ──
let currentUser = null;
let devEnabled = false;   // DEV mode flag from /api/config
let devUser = null;       // Slack ID being previewed in DEV mode
let devUsers = [];        // [{slack_id, display_name}] for the DEV autocomplete
let priorityReviewEnabled = false;
let priorityRequested = new Set();
let priorityProjects = [];

async function checkAuth() {
  try {
    const r = await fetch('/api/auth/me');
    if (r.ok) {
      currentUser = await r.json();
      renderUser();
      await loadConfig();
      loadMyProjects();
    } else {
      currentUser = null;
      renderUser();
    }
  } catch { currentUser = null; renderUser(); }
}

async function loadConfig() {
  try {
    const r = await fetch('/api/config');
    const d = await r.json();
    priorityReviewEnabled = !!d.priority_review_enabled;
  } catch { priorityReviewEnabled = false; }
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
  updatePriorityBtn();
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
  const titleText = document.getElementById('island-title-text');
  if (titleText) titleText.textContent = devUser ? `Projects · ${devUser}` : 'My Project Approvals';
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

    // All queue projects are eligible for priority review. Projects with an
    // existing request (pending/approved/rejected) come back flagged and stay
    // locked until they clear regular review.
    priorityRequested = new Set();
    priorityProjects = projects.filter(p => p.source === 'queue');
    for (const p of projects) {
      if (p.priorityReviewRequested) {
        priorityRequested.add(p.projectId);
      }
    }
    updatePriorityBtn();

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

        const title = escHtml(p.projectTitle || '(untitled)');
        const titleHtml = (p.projectId != null)
          ? `<a class="project-link" href="https://horizons.hackclub.com/projects/${encodeURIComponent(p.projectId)}" target="_blank" rel="noopener">${title}</a>`
          : title;

        return `<div class="project-item">
          <div class="project-row">
            <div class="project-info">
              <div class="project-title">${titleHtml}</div>
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
  if (p.source === 'queue') return '';
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
  let isDev = false;
  try {
    const r = await fetch('/api/config');
    const d = await r.json();
    devEnabled = !!d.impersonate;   // DEV mode OR a logged-in admin
    isDev = !!d.dev;
  } catch { devEnabled = false; }
  if (!devEnabled) return;

  const box = document.getElementById('dev-box');
  const input = document.getElementById('dev-user');
  const tag = box.querySelector('.dev-tag');
  if (tag) tag.textContent = isDev ? 'DEV' : 'ADMIN';
  box.style.display = '';

  // The all-users list is expensive to build, so only fetch it the first time
  // the box is focused (autocomplete: "Display Name · <slackId>"). apply() pulls
  // the trailing Slack ID back out, and raw IDs work without the list at all.
  let usersLoaded = false;
  input.addEventListener('focus', () => {
    if (usersLoaded) return;
    usersLoaded = true;
    loadDevUsers();
  });

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

// ── Priority Review ──

function updatePriorityBtn() {
  const btn = document.getElementById('priority-review-btn');
  if (!btn) return;
  const hasPending = priorityProjects.some(p => !priorityRequested.has(p.projectId));
  btn.disabled = !(currentUser && hasPending);
}

function populatePriorityDropdown() {
  const select = document.getElementById('priority-project');
  select.innerHTML = '';
  const available = priorityProjects.filter(p => !priorityRequested.has(p.projectId));
  if (!available.length) {
    const opt = document.createElement('option');
    opt.textContent = 'No projects available';
    opt.disabled = true;
    opt.selected = true;
    select.appendChild(opt);
    return;
  }
  for (const p of available) {
    const opt = document.createElement('option');
    opt.value = p.projectId;
    opt.textContent = p.projectTitle || `Project #${p.projectId}`;
    select.appendChild(opt);
  }
}

function openPriorityDisclaimer() {
  document.getElementById('priority-disclaimer-modal').style.display = '';
}

function closePriorityDisclaimer() {
  document.getElementById('priority-disclaimer-modal').style.display = 'none';
}

function openPriorityModal() {
  closePriorityDisclaimer();
  document.getElementById('priority-error').style.display = 'none';
  document.getElementById('priority-success').style.display = 'none';
  document.getElementById('priority-form').style.display = '';
  document.getElementById('priority-reason').value = '';
  document.getElementById('reason-count').textContent = '0';
  populatePriorityDropdown();
  document.getElementById('priority-modal').style.display = '';
}

function closePriorityModal() {
  document.getElementById('priority-modal').style.display = 'none';
}

document.getElementById('priority-review-btn').addEventListener('click', openPriorityDisclaimer);

document.getElementById('priority-disclaimer-cancel').addEventListener('click', closePriorityDisclaimer);
document.getElementById('priority-disclaimer-continue').addEventListener('click', openPriorityModal);

document.getElementById('priority-cancel').addEventListener('click', closePriorityModal);

document.getElementById('priority-close').addEventListener('click', closePriorityModal);

document.getElementById('priority-reason').addEventListener('input', function () {
  document.getElementById('reason-count').textContent = this.value.length;
});

document.getElementById('priority-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const select = document.getElementById('priority-project');
  const projectId = parseInt(select.value, 10);
  const reason = document.getElementById('priority-reason').value.trim();
  const errorEl = document.getElementById('priority-error');
  const submitBtn = document.getElementById('priority-submit');

  if (!projectId || !reason) {
    errorEl.textContent = 'Please select a project and provide a reason.';
    errorEl.style.display = '';
    return;
  }

  errorEl.style.display = 'none';
  submitBtn.disabled = true;
  submitBtn.textContent = 'Submitting...';

  try {
    const r = await fetch('/api/priority-review', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ project_id: projectId, reason }),
    });
    const data = await r.json();
    if (!r.ok) {
      throw new Error(data.error || 'Request failed');
    }
    priorityRequested.add(projectId);
    document.getElementById('priority-form').style.display = 'none';
    document.getElementById('priority-success').style.display = '';
    updatePriorityBtn();
    loadMyProjects();
  } catch (err) {
    errorEl.textContent = err.message;
    errorEl.style.display = '';
  } finally {
    submitBtn.disabled = false;
    submitBtn.textContent = 'Submit Request';
  }
});

// Click modal overlay to close
document.getElementById('priority-disclaimer-modal').addEventListener('click', (e) => {
  if (e.target === e.currentTarget) closePriorityDisclaimer();
});
document.getElementById('priority-modal').addEventListener('click', (e) => {
  if (e.target === e.currentTarget) closePriorityModal();
});

// ── Admin: Priority Review Queue ──

async function initAdminPanel() {
  let isAdmin = false;
  try {
    const r = await fetch('/api/config');
    const d = await r.json();
    isAdmin = !!d.impersonate;
  } catch { return; }
  if (!isAdmin) return;

  const card = document.getElementById('admin-card');
  const skel = document.getElementById('admin-skel');
  const cont = document.getElementById('admin-content');
  if (!card || !skel || !cont) return;
  card.style.display = '';

  async function loadAdmin() {
    try {
      const r = await fetch('/api/priority-review/admin');
      if (!r.ok) throw new Error(await r.text());
      const data = await r.json();
      const entries = data.entries || [];
      skel.style.display = 'none';
      cont.style.display = '';
      if (!entries.length) {
        cont.innerHTML = '<div class="island-empty">No priority review requests.</div>';
        return;
      }
      cont.innerHTML = entries.map(e => {
        const name = escHtml(e.display_name || e.slack_id);
        const title = escHtml(e.project_title || `Project #${e.projectId}`);
        const reason = escHtml((e.reason || '').slice(0, 120));
        const status = e.status || 'unknown';
        const statusClass = status === 'approved' ? 'badge-status-approved'
          : status === 'rejected' ? 'badge-status-rejected'
          : 'badge-status-pending';
        const reviewUrl = `https://horizons.hackclub.com/admin/review/${e.project_id}`;
        return `<div class="project-item">
          <div class="project-row" style="flex-wrap:wrap;gap:4px 12px">
            <div class="project-info" style="flex:1;min-width:200px">
              <div class="project-title">
                <a class="project-link" href="${reviewUrl}" target="_blank" rel="noopener">${title}</a>
              </div>
              <div class="project-meta">${name}</div>
            </div>
            <div class="project-status" style="gap:6px">
              <span class="badge ${statusClass}">${status}</span>
              <a class="hca-btn" style="font-size:12px;padding:4px 10px;text-decoration:none" href="${reviewUrl}" target="_blank" rel="noopener">Review →</a>
            </div>
          </div>
          ${reason ? `<div class="project-feedback" style="margin-top:2px"><span class="feedback-label">Reason</span><span class="feedback-text">${reason}${e.reason.length > 120 ? '…' : ''}</span></div>` : ''}
        </div>`;
      }).join('');
    } catch (e) {
      console.error('Failed to load priority review admin:', e);
      skel.style.display = '';
      cont.style.display = 'none';
    }
  }

  await loadAdmin();
  setInterval(loadAdmin, 30000);
  initReviewerHours();
  initAdminManagement();
  initSessionIdCard();
}

function initReviewerHours() {
  const card = document.getElementById('hours-card');
  const input = document.getElementById('hours-name-input');
  const startDate = document.getElementById('hours-start-date');
  const endDate = document.getElementById('hours-end-date');
  const btn = document.getElementById('hours-load-btn');
  const skel = document.getElementById('hours-skel');
  const cont = document.getElementById('hours-content');
  if (!card || !input || !btn || !skel || !cont) return;
  card.style.display = '';

  async function loadHours(name) {
    if (!name) return;
    skel.style.display = '';
    cont.style.display = 'none';
    cont.innerHTML = '';
    try {
      let url = '/api/reviewer/hours?name=' + encodeURIComponent(name);
      if (startDate.value) url += '&startDate=' + encodeURIComponent(startDate.value);
      if (endDate.value) url += '&endDate=' + encodeURIComponent(endDate.value);
      const r = await fetch(url);
      if (!r.ok) {
        const err = await r.json();
        cont.innerHTML = '<div class="island-empty">' + escHtml(err.error || 'Error') + '</div>';
        skel.style.display = 'none';
        cont.style.display = '';
        return;
      }
      const data = await r.json();
      skel.style.display = 'none';
      cont.style.display = '';
      if (!data.events || !data.events.length) {
        cont.innerHTML = '<div class="island-empty">No reviewed hours yet.</div>';
        return;
      }
      cont.innerHTML = '<div class="events-grid">' +
        data.events.map(e => `<div class="event-blob">
          <div class="event-blob-name">${escHtml(e.title)}</div>
          <div class="event-stat">
            <div class="event-stat-num">${e.reviews}</div>
            <div class="event-stat-label">reviews</div>
          </div>
          <div class="event-stat">
            <div class="event-stat-num event-stat-num-sm">${Math.round(e.hours)}</div>
            <div class="event-stat-label">hours</div>
          </div>
        </div>`).join('') +
        '</div>';
    } catch (e) {
      console.error('Failed to load reviewer hours:', e);
      skel.style.display = '';
      cont.style.display = 'none';
    }
  }

  btn.addEventListener('click', () => loadHours(input.value.trim()));
  input.addEventListener('keydown', e => { if (e.key === 'Enter') loadHours(input.value.trim()); });
}

// ── Admin: Manage file-based admin users ──

async function initAdminManagement() {
  const card = document.getElementById('admin-users-card');
  const skel = document.getElementById('admin-users-skel');
  const cont = document.getElementById('admin-users-content');
  if (!card || !skel || !cont) return;
  card.style.display = '';

  async function loadAdminUsers() {
    try {
      const r = await fetch('/api/admin/users');
      if (!r.ok) throw new Error(await r.text());
      const data = await r.json();
      skel.style.display = 'none';
      cont.style.display = '';

      const envAdmins = data.env_admins || [];
      const fileAdmins = data.file_admins || [];

      let html = '';

      if (envAdmins.length) {
        html += '<div class="admin-section-label">Environment Admins</div>';
        html += envAdmins.map(id =>
          '<div class="admin-user-row"><span class="admin-user-id">' + escHtml(id) + '</span><span class="badge badge-status-pending" style="font-size:10px">env</span></div>'
        ).join('');
      }

      if (fileAdmins.length) {
        html += '<div class="admin-section-label">File-based Admins</div>';
        html += fileAdmins.map(id =>
          '<div class="admin-user-row"><span class="admin-user-id">' + escHtml(id) + '</span><button class="admin-remove-btn" data-slack-id="' + escHtml(id) + '">Remove</button></div>'
        ).join('');
      }

      if (!envAdmins.length && !fileAdmins.length) {
        html += '<div class="admin-empty">No admin users configured.</div>';
      }

      html += '<div class="admin-add-row">';
      html += '<input id="admin-add-input" type="text" placeholder="Enter Slack ID…" spellcheck="false" class="admin-input">';
      html += '<button id="admin-add-btn" class="admin-btn admin-btn-primary">Add Admin</button>';
      html += '</div>';

      cont.innerHTML = html;

      document.getElementById('admin-add-btn').addEventListener('click', async () => {
        const input = document.getElementById('admin-add-input');
        const id = input.value.trim();
        if (!id) return;
        try {
          await fetch('/api/admin/users', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ slack_id: id })
          });
          input.value = '';
          loadAdminUsers();
        } catch (e) {
          console.error('Failed to add admin:', e);
        }
      });
      document.getElementById('admin-add-input').addEventListener('keydown', e => {
        if (e.key === 'Enter') document.getElementById('admin-add-btn').click();
      });

      cont.querySelectorAll('.admin-remove-btn').forEach(btn => {
        btn.addEventListener('click', async () => {
          const id = btn.dataset.slackId;
          try {
            await fetch('/api/admin/users?slack_id=' + encodeURIComponent(id), { method: 'DELETE' });
            loadAdminUsers();
          } catch (e) {
            console.error('Failed to remove admin:', e);
          }
        });
      });
    } catch (e) {
      console.error('Failed to load admin users:', e);
      skel.style.display = '';
      cont.style.display = 'none';
    }
  }

  await loadAdminUsers();
}

// ── Priority Review Stats ──

function renderPrBar(total, segs) {
  const pct = v => total > 0 ? (v / total) * 100 : 0;
  for (const s of segs) {
    const el = document.getElementById(s.id);
    el.style.width = pct(s.v) + '%';
    el.querySelector('span').textContent = s.v > 0 ? s.v : '';
  }
}

async function loadPriorityReviewStats() {
  const skel = document.getElementById('pr-stats-skel');
  const cont = document.getElementById('pr-stats-content');
  try {
    const r = await fetch('/api/priority-review/stats');
    if (!r.ok) throw new Error(await r.text());
    const d = await r.json();
    skel.style.display = 'none';
    cont.style.display = '';

    const prTotal = d.pr_pending + d.pr_approved + d.pr_rejected;
    if (prTotal > 0) {
      document.getElementById('pr-stats-total-row').style.display = '';
      document.getElementById('pr-stats-total-num').textContent = prTotal;
    }
    renderPrBar(prTotal, [
      { id: 'pr-seg-pending',  v: d.pr_pending },
      { id: 'pr-seg-approved',  v: d.pr_approved },
      { id: 'pr-seg-rejected',  v: d.pr_rejected },
    ]);

  } catch (e) {
    console.error('Failed to load priority review stats:', e);
    skel.style.display = '';
    cont.style.display = 'none';
  }
}

// ── Admin: Session ID Override ──

let sessionIdDisclaimerShown = false;

function openSessionIdDisclaimer() {
  document.getElementById('session-id-disclaimer-modal').style.display = '';
}

function closeSessionIdDisclaimer() {
  document.getElementById('session-id-disclaimer-modal').style.display = 'none';
}

document.getElementById('session-id-disclaimer-cancel').addEventListener('click', closeSessionIdDisclaimer);
document.getElementById('session-id-disclaimer-continue').addEventListener('click', () => {
  closeSessionIdDisclaimer();
  sessionIdDisclaimerShown = true;
  showSessionIdInput();
});

document.getElementById('session-id-disclaimer-modal').addEventListener('click', (e) => {
  if (e.target === e.currentTarget) closeSessionIdDisclaimer();
});

function showSessionIdInput() {
  const cont = document.getElementById('session-id-content');
  if (cont) cont.style.display = '';
}

async function initSessionIdCard() {
  const card = document.getElementById('session-id-card');
  const skel = document.getElementById('session-id-skel');
  const cont = document.getElementById('session-id-content');
  if (!card || !skel || !cont) return;
  card.style.display = '';

  async function loadOverrideStatus() {
    try {
      const r = await fetch('/api/admin/session-id');
      if (!r.ok) throw new Error();
      const d = await r.json();
      skel.style.display = 'none';
      cont.style.display = '';
      const statusEl = document.getElementById('session-id-status');
      if (d.overridden) {
        statusEl.textContent = 'Session ID has been overridden (active until reset or restart).';
        statusEl.className = 'session-id-status error';
      } else {
        statusEl.textContent = 'Using original HORIZONS_SESSION_ID from environment.';
        statusEl.className = 'session-id-status';
      }
    } catch {
      skel.style.display = 'none';
      cont.style.display = '';
    }
  }

  document.getElementById('session-id-apply-btn').addEventListener('click', async () => {
    const input = document.getElementById('session-id-input');
    const sid = input.value.trim();
    if (!sid) return;

    if (!sessionIdDisclaimerShown) {
      openSessionIdDisclaimer();
      return;
    }

    try {
      const r = await fetch('/api/admin/session-id', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ session_id: sid }),
      });
      if (!r.ok) {
        const err = await r.json();
        alert('Failed: ' + (err.error || 'unknown error'));
        return;
      }
      input.value = '';
      await loadOverrideStatus();
    } catch (e) {
      alert('Failed: ' + e.message);
    }
  });

  document.getElementById('session-id-clear-btn').addEventListener('click', async () => {
    try {
      const r = await fetch('/api/admin/session-id', { method: 'DELETE' });
      if (!r.ok) {
        const err = await r.json();
        alert('Failed: ' + (err.error || 'unknown error'));
        return;
      }
      await loadOverrideStatus();
    } catch (e) {
      alert('Failed: ' + e.message);
    }
  });

  await loadOverrideStatus();
}

loadStats();
setInterval(loadStats, 30000);
loadEvents();
setInterval(loadEvents, 30000);
loadPriorityReviewStats();
setInterval(loadPriorityReviewStats, 30000);
checkAuth();
initDevBox();
initAdminPanel();
