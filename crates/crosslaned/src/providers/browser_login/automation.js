function step(options) {
  const { credentials, githubOrigin, verificationUri, userCode, otp } = options;
  const visible = el => !!el && !el.disabled && el.offsetParent !== null;
  const first = selectors => selectors.map(selector => document.querySelector(selector)).find(visible);
  const state = stage => ({ stage });
  const text = document.body?.innerText || '';
  const origin = location.origin;
  const github = origin === githubOrigin;
  const azure = ['https://login.microsoftonline.com', 'https://login.microsoft.com', 'https://login.windows.net'].includes(origin);
  // Enrollment changes account security settings; it is not an MFA challenge.
  if (origin === 'https://mysignins.microsoft.com') return state('security_info_required');
  // Passwords are origin-specific; an arbitrary redirect must never receive them.
  if (!github && !azure) return state('untrusted_origin');
  if (azure && /you cannot access this right now|you can.?t get there from here|无法立即访问此资源|不符合访问此资源的条件|(?:AADSTS|Error Code:\s*)530(?:00|01|02|03|04|09)\b/i.test(text)) {
    return state('conditional_access_denied');
  }
  if (azure && (new URLSearchParams(location.search || '').get('claims') || '').includes('urn:user:registersecurityinfo')) {
    return state('security_info_required');
  }
  if (azure && /more information required|keep your account secure|需要更多信息|保护你的帐户|保护您的帐户/i.test(text)
      && !first(['input[name="loginfmt"]', 'input[name="passwd"]'])) return state('security_info_required');
  const memory = window.__crosslaneDeviceFlow || (window.__crosslaneDeviceFlow = { clicks: {}, usernameAt: 0 });
  const fill = (el, value) => {
    if (!visible(el) || value === undefined || value === null || el.value === value) return false;
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(el, value);
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
    el.dispatchEvent(new FocusEvent('blur', { bubbles: true }));
    return true;
  };
  const click = (el, key) => {
    if (!visible(el) || memory.clicks[key]) return false;
    memory.clicks[key] = true;
    el.click();
    return true;
  };
  const button = regex => [...document.querySelectorAll('button,input[type=submit],a')]
    .find(el => visible(el) && regex.test((el.innerText || el.value || '').trim()));
  if (first(['input[autocomplete="new-password"]', 'input[name="new_password"]', '#newPassword'])) return state('password_change');
  if (first(['#captcha', 'iframe[src*="captcha"]', 'iframe[src*="arkoselabs"]'])) return state('captcha');

  const mfa = first(['input[name="otc"]', 'input[name="app_otp"]', 'input[name="sms_otp"]', 'input[autocomplete="one-time-code"]']);
  if (mfa && !location.pathname.startsWith('/login/device')) {
    if (otp) {
      fill(mfa, otp);
      // A new code is an explicit user action; permit resubmission after a rejected code.
      const submit = first(['#idSubmit_SAOTCC_Continue', '#idSIButton9', 'button[type=submit]', 'input[type=submit]']);
      if (submit) submit.click();
      return state('mfa_submitted');
    }
    return state('mfa_code');
  }
  if (first(['#passwordError', '#usernameError', '#service_exception_message', '#errorText', '.flash-error'])) return state('login_rejected');
  if (azure && /approve (the |this |a )?(sign.?in|request)|check your.*authenticator|批准.*(登录|请求)/i.test(text)) {
    const number = (first(['#idRichContext_DisplaySign'])?.innerText || '').trim();
    return { stage: 'mfa_push', number };
  }
  if (/insert.*security key|touch.*security key|use your passkey|验证.*安全密钥/i.test(text)
      && !first(['input[type=password]', 'input[name=loginfmt]'])) return state('security_key');

  if (azure) {
    if (!credentials.sso_username || !credentials.sso_password) return state('sso_credentials_required');
    const user = first(['input[name="loginfmt"]', '#i0116', 'input[type=email]']);
    if (user) {
      fill(user, credentials.sso_username);
      click(first(['#idSIButton9', 'input[type=submit]', 'button[type=submit]']), 'azure-user');
      return state('sso_username');
    }
    const password = first(['input[name="passwd"]', '#i0118', 'input[type=password]']);
    if (password) {
      fill(password, credentials.sso_password);
      click(first(['#idSIButton9', 'input[type=submit]', 'button[type=submit]']), 'azure-password');
      return state('sso_password');
    }
    if (/stay signed in|保持登录/i.test(text) && click(first(['#idBtn_Back']), 'kmsi')) return state('stay_signed_in');
    return state('waiting');
  }

  const user = first(['input[name="login"]', '#login_field']);
  if (user) {
    if (!memory.usernameAt) {
      fill(user, credentials.github_username);
      memory.usernameAt = Date.now();
      return state('github_username');
    }
    const sso = button(/sign in with (your )?identity provider|sign in with single sign.on|使用.*身份提供商/i);
    if (sso) {
      click(sso, 'github-sso');
      return state('sso_redirect');
    }
    const password = first(['#password', 'input[name="password"]']);
    if (password && credentials.github_password) {
      fill(password, credentials.github_password);
      click(button(/^sign in$|^登录$/i), 'github-password');
      return state('github_password');
    }
    if (password && Date.now() - memory.usernameAt > 15000) return state('github_password_required');
    return state('github_username');
  }
  const sso = button(/sign in with (your )?identity provider|single sign.on/i);
  if (sso && click(sso, 'github-sso')) return state('sso_redirect');

  // GitHub's enterprise OIDC interstitial uses a plain "Continue" button.
  // Match its same-origin form action, not arbitrary Continue buttons.
  if (/^\/(enterprises|orgs)\/[^/]+\/sso$/.test(location.pathname)) {
    const form = [...document.forms].find(form => {
      const action = new URL(form.action, location.href);
      return form.method.toLowerCase() === 'post' && action.origin === githubOrigin
        && /^\/(enterprises|orgs)\/[^/]+\/(oidc|saml)\/initiate$/.test(action.pathname);
    });
    const continueButton = form?.querySelector('button[type=submit],input[type=submit]');
    if (click(continueButton, 'github-sso-continue')) return state('sso_redirect');
  }

  if (location.pathname.startsWith('/login/device')) {
    if (location.pathname === '/login/device/select_account') {
      const confirm = [...document.querySelectorAll('button[type=submit],input[type=submit]')].find(el => {
        if (!visible(el) || !el.form) return false;
        const action = new URL(el.form.action, location.href);
        return el.form.method.toLowerCase() === 'post' && action.origin === githubOrigin
          && action.pathname === '/login/device/select_account';
      });
      if (confirm) {
        const login = (confirm.getAttribute('aria-label') || '').match(/^Continue as (.+)$/i)?.[1];
        if (!login) return state('account_confirmation_required');
        if (login.toLowerCase() !== credentials.github_username.toLowerCase()) return state('account_mismatch');
        click(confirm, 'select-account');
        return state('select_account');
      }
      return state('waiting');
    }
    const codeInput = first(['input[name="user_code"]', '#user_code', '#user-code']);
    const slots = [...document.querySelectorAll('input[maxlength="1"]')].filter(visible);
    if (codeInput || slots.length === 8) {
      if (codeInput) fill(codeInput, userCode);
      else slots.forEach((el, index) => fill(el, userCode.replace(/-/g, '')[index]));
      click(button(/^continue$|^submit$|^继续$|^提交$/i), 'device-code');
      return state('device_code');
    }
    const authorize = first(['#js-oauth-authorize-btn', 'button[name="authorize"]', 'button[value="authorize"]'])
      || button(/^authorize\b|^授权/iu);
    if (authorize) {
      click(authorize, 'authorize');
      return state('authorize');
    }
    if (/congratulations|you.?re all set|device is now connected|successfully authorized|授权成功/i.test(text)) return state('complete');
  }
  if (location.pathname.startsWith('/login/oauth/authorize')) {
    const authorize = first(['#js-oauth-authorize-btn', 'button[name="authorize"]']) || button(/^authorize\b/iu);
    if (authorize) { click(authorize, 'authorize'); return state('authorize'); }
  }
  // An optional enterprise SSO entry may land on its home page after login.
  if (credentials.sso_start_url && !location.pathname.startsWith('/login/')
      && !location.pathname.endsWith('/sso') && !memory.clicks.returned
      && document.querySelector('meta[name="user-login"]')?.content) {
    memory.clicks.returned = true;
    location.assign(verificationUri);
    return state('return_to_device');
  }
  return state('waiting');
}
