const PORT = 19842;

const btn = document.getElementById('btn');
const status = document.getElementById('status');

btn.addEventListener('click', async () => {
  btn.disabled = true;
  status.textContent = 'Читаю cookies…';

  let cookies;
  try {
    cookies = await new Promise(resolve =>
      chrome.cookies.getAll({ domain: 'claude.ai' }, resolve)
    );
  } catch (e) {
    status.textContent = 'Ошибка доступа к cookies.';
    btn.disabled = false;
    return;
  }

  if (!cookies || !cookies.length) {
    status.textContent = 'Войди в claude.ai сначала.';
    btn.disabled = false;
    return;
  }

  const sameSiteMap = {
    no_restriction: 'None', lax: 'Lax', strict: 'Strict', unspecified: 'None'
  };

  const playwright = cookies.map(c => ({
    name: c.name,
    value: c.value,
    domain: c.domain,
    path: c.path || '/',
    expires: c.expirationDate != null ? Math.round(c.expirationDate) : -1,
    httpOnly: c.httpOnly || false,
    secure: c.secure || false,
    sameSite: sameSiteMap[(c.sameSite || 'unspecified').toLowerCase()] || 'None'
  }));

  try {
    const resp = await fetch(`http://localhost:${PORT}/auth`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ cookies: playwright })
    });
    if (resp.ok) {
      status.textContent = '✓ Готово!';
      setTimeout(() => window.close(), 700);
    } else {
      status.textContent = 'Приложение не ждёт. Нажми "Войти" сначала.';
      btn.disabled = false;
    }
  } catch (e) {
    status.textContent = 'Приложение не запущено.';
    btn.disabled = false;
  }
});
