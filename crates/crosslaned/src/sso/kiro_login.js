// IAM Identity Center automation for Kiro. Copilot supplies its own driver.
(email, password) => {
  const visible = el => !!el && !el.disabled && el.offsetParent !== null;
  const first = selectors => selectors.map(s => document.querySelector(s)).find(visible);
  // An organization may require an interactive credential update. This is not
  // the ordinary login form: never fill its current/new/confirmation fields.
  const newPassword = first(['#newPassword', 'input[name="newpasswd"]', 'input[name="newPassword"]', 'input[autocomplete="new-password"]']);
  const confirmPassword = first(['#confirmNewPassword', 'input[name="confirmnewpasswd"]', 'input[name="confirmPassword"]']);
  if (newPassword && confirmPassword) return 'blocked:password-update-required';

  const errors = [...document.querySelectorAll('#passwordError,#usernameError,[role="alert"],.error,.error-message,.alert-error')]
    .filter(visible).map(el => el.innerText || '').join(' ');
  if (/account.{0,30}locked|too many.{0,30}attempts|too many.{0,30}tried|账户.{0,10}锁定/i.test(errors)) return 'blocked:account-locked';
  if (/account or password is incorrect|incorrect.{0,20}password|password.{0,20}incorrect|password.{0,20}isn.t correct|密码不正确|密码有误/i.test(errors)) return 'blocked:invalid-credentials';

  const setValue = (el, value) => {
    if (!visible(el) || el.value) return false;
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
    setter.call(el, value);
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
  };
  const user = first(['input[name="loginfmt"]','input[name="username"]','input[type="email"]','input[name="email"]','#i0116','#username','#email','input[autocomplete="username"]','input[autocomplete="email"]']);
  if (setValue(user, email)) {
    const next = first(['#idSIButton9','button[type="submit"]','input[type="submit"]']);
    if (next) next.click();
    return 'username';
  }
  const pass = first(['input[name="passwd"]','input[type="password"]','input[name="password"]','#i0118','#passwordInput','#password']);
  if (setValue(pass, password)) {
    const submit = first(['#idSIButton9','button[type="submit"]','input[type="submit"]']);
    if (submit) submit.click();
    return 'password';
  }
  const no = first(['#idBtn_Back']);
  if (no && /stay signed in/i.test(document.body.innerText)) { no.click(); return 'kmsi'; }
  const allow = first(['button[name="allow"]','[data-testid="allow-button"]','[data-testid="allow-access-button"]','button[value="allow"]','#allow-button','button.awsui-button--primary','#cli_login_button','button[data-analytics="allowButton"]']);
  if (allow) { allow.click(); return 'allow'; }
  const textAllow = [...document.querySelectorAll('button,input[type="submit"]')]
    .find(el => visible(el) && /^(allow|authorize|allow access|continue|confirm|允许|确认授权)/i.test((el.innerText || el.value || '').trim()));
  if (textAllow) { textAllow.click(); return 'allow-text'; }
  const visibleInputs = [...document.querySelectorAll('input')].filter(visible);
  const hasOtp = visibleInputs.some(el =>
    /otp|code|verification|one.?time/i.test(`${el.name} ${el.id} ${el.autocomplete}`));
  if (hasOtp) return 'waiting:mfa';
  if (errors.trim()) return 'waiting:error';
  if (visibleInputs.length) return 'waiting:unrecognized-input';
  return 'waiting:page';
}
