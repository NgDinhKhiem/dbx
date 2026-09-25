<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { Button } from "@/components/ui/button";
import PasswordInput from "@/components/ui/PasswordInput.vue";
import { KeyRound, Lock, Loader2, ShieldCheck } from "@lucide/vue";
import AppLogo from "@/components/icons/AppLogo.vue";
import { apiUrl } from "@/lib/common/webPath";
import { translateBackendError } from "@/i18n/backend-errors";

const props = withDefaults(
  defineProps<{
    setupMode?: boolean;
  }>(),
  { setupMode: false },
);

const emit = defineEmits<{ authenticated: [] }>();
const { t } = useI18n();

const password = ref("");
const confirmPassword = ref("");
const setupToken = ref("");
// Remote first-run setup needs the one-time token from the server log; a
// browser on the server itself may skip it (reported by /api/auth/check).
const setupTokenRequired = ref(true);
const error = ref("");
const loading = ref(false);

onMounted(async () => {
  if (!props.setupMode) return;
  try {
    const res = await fetch(apiUrl("/api/auth/check"), { credentials: "same-origin" });
    if (!res.ok) return;
    const data = await res.json();
    if (data && data.setup_token_required === false) setupTokenRequired.value = false;
  } catch {
    // keep the token field visible
  }
});

// Auth routes add a machine-readable `code` to some failures.
const authErrorCodes: Record<string, string> = {
  SETUP_TOKEN_INVALID: "auth.setupTokenInvalid",
  HOST_NOT_ALLOWED: "auth.hostNotAllowed",
};

// The auth routes report failures as `{"error": "..."}`, so unwrap that before
// translating; anything else is treated as a plain-text message.
async function readAuthError(res: Response): Promise<string> {
  const text = (await res.text()).trim();
  if (!text) return t("auth.loginFailed");
  let message = text;
  try {
    const parsed = JSON.parse(text);
    if (parsed && typeof parsed.code === "string" && authErrorCodes[parsed.code]) return t(authErrorCodes[parsed.code]);
    if (parsed && typeof parsed.error === "string") message = parsed.error;
  } catch {
    // not JSON — fall through with the raw body
  }
  return translateBackendError(t, message) || t("auth.loginFailed");
}

async function submit() {
  if (props.setupMode && password.value !== confirmPassword.value) {
    error.value = t("auth.passwordMismatch");
    return;
  }

  loading.value = true;
  error.value = "";
  try {
    const url = apiUrl(props.setupMode ? "/api/auth/setup" : "/api/auth/login");
    const res = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(props.setupMode && setupToken.value.trim() ? { password: password.value, setup_token: setupToken.value.trim() } : { password: password.value }),
    });
    if (res.ok) {
      emit("authenticated");
    } else {
      error.value = await readAuthError(res);
    }
  } catch (e: any) {
    error.value = e?.message || t("auth.connectFailed");
  } finally {
    loading.value = false;
  }
}
</script>

<template>
  <div class="flex items-center justify-center h-screen bg-gradient-to-br from-background via-background to-blue-950/20">
    <div class="w-[360px] space-y-8">
      <div class="flex flex-col items-center gap-4">
        <AppLogo class="w-20 h-20 rounded-2xl shadow-lg shadow-blue-500/20" />
        <div class="text-center">
          <h1 class="text-2xl font-bold tracking-tight">DBX</h1>
          <p class="text-sm text-muted-foreground mt-1">
            {{ setupMode ? t("auth.setupDescription") : t("auth.loginDescription") }}
          </p>
        </div>
      </div>

      <form class="space-y-4" @submit.prevent="submit" autocomplete="off">
        <div v-if="setupMode" class="flex items-center justify-center gap-2 text-sm text-muted-foreground">
          <ShieldCheck class="w-4 h-4" />
          <span>{{ t("auth.setupTitle") }}</span>
        </div>
        <div v-if="setupMode && setupTokenRequired" class="space-y-1">
          <div class="relative">
            <KeyRound class="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-muted-foreground" />
            <PasswordInput v-model="setupToken" :placeholder="t('auth.setupToken')" inputClass="pl-10 h-11 font-mono" autocomplete="off" />
          </div>
          <p class="text-xs text-muted-foreground">{{ t("auth.setupTokenHint") }}</p>
        </div>
        <div class="relative">
          <Lock class="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-muted-foreground" />
          <PasswordInput v-model="password" :placeholder="setupMode ? t('auth.newPassword') : t('auth.enterPassword')" inputClass="pl-10 h-11" autocomplete="off" autofocus />
        </div>
        <div v-if="setupMode" class="relative">
          <Lock class="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-muted-foreground" />
          <PasswordInput v-model="confirmPassword" :placeholder="t('auth.confirmPassword')" inputClass="pl-10 h-11" autocomplete="off" />
        </div>
        <p v-if="error" class="text-sm text-destructive text-center">{{ error }}</p>
        <Button type="submit" class="w-full h-11 text-sm font-medium" :disabled="loading || !password || (setupMode && !confirmPassword) || (setupMode && setupTokenRequired && !setupToken.trim())">
          <Loader2 v-if="loading" class="w-4 h-4 animate-spin mr-2" />
          {{ loading ? t("auth.processing") : setupMode ? t("auth.setPassword") : t("auth.login") }}
        </Button>
      </form>

      <p class="text-center text-xs text-muted-foreground/50">Powered by DBX</p>
    </div>
  </div>
</template>
