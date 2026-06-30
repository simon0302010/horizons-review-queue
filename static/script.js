// ── Auth state ──
let currentUser = null;
let impersonating = null;
let allUsers = [];

async function checkAuth() {
  try {
    const r = await fetch('/api/auth/me');
    if (r.ok) {
      currentUser = await r.json();
      renderUser();
      loadMyProjects();
      if (currentUser.debug) initDebugPanel();
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

  if (!currentUser) {
    content.innerHTML = '<div class="island-empty">Log in to see your projects review status.</div>';
    return;
  }

  island.classList.add('open');
  content.innerHTML = '<div class="island-loading">Loading your projects...</div>';

  try {
    const url = impersonating ? `/api/my/projects?impersonate=${encodeURIComponent(impersonating)}` : '/api/my/projects';
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
          <div class="project-info">
            <div class="project-title">${escHtml(p.projectTitle || '(untitled)')}</div>
            <div class="project-meta">${escHtml(meta)}</div>
          </div>
          <div class="project-status">
            ${mainBadges}
          </div>
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

loadStats();
setInterval(loadStats, 30000);
loadEvents();
setInterval(loadEvents, 30000);
loadQueue();
setInterval(loadQueue, 30000);
checkAuth();

// ── Queue list ──
async function loadQueue() {
  const skel = document.getElementById('queue-skel');
  const cont = document.getElementById('queue-content');
  try {
    const r = await fetch('/api/queue/normal');
    if (!r.ok) throw new Error(await r.text());
    const items = await r.json();
    if (!items.length) {
      skel.style.display = 'none';
      cont.style.display = '';
      cont.innerHTML = '<div class="island-empty">No projects in normal review.</div>';
      return;
    }
    skel.style.display = 'none';
    cont.style.display = '';
    cont.innerHTML = items.map(function (item) {
      var claimed = item.claimed ? '<span class="badge badge-claimed" style="margin-left:10px">Claimed</span>' : '';
      var type = (item.projectType || '').replace(/_/g, ' ');
      return '<div class="queue-item">' +
        '<span class="queue-pos">#' + item.position + '</span>' +
        '<span class="queue-title">' + escHtml(item.projectTitle || '(untitled)') + '</span>' +
        '<span class="queue-user">' + escHtml(item.slackUserId || '') + '</span>' +
        '<span class="queue-type">' + escHtml(type) + '</span>' +
        claimed +
        '</div>';
    }).join('');
  } catch (e) {
    console.error('Failed to load queue:', e);
    skel.style.display = '';
    cont.style.display = 'none';
  }
}

// ── Debug: Impersonation panel ──
async function initDebugPanel() {
  document.getElementById('debug-panel').style.display = '';

  var input = document.getElementById('debug-slack-id');
  var btn = document.getElementById('debug-impersonate-btn');

  function doImpersonate() {
    var sid = input.value.trim();
    if (!sid) {
      impersonating = null;
      document.getElementById('debug-current').style.display = 'none';
      loadMyProjects();
      return;
    }
    impersonating = sid;
    var display = document.getElementById('debug-current');
    display.textContent = 'Impersonating: ' + sid;
    display.style.display = '';
    loadMyProjects();
  }

  btn.addEventListener('click', doImpersonate);
  input.addEventListener('keydown', function (e) {
    if (e.key === 'Enter') doImpersonate();
  });
}
