// Runtime probe for the Android on-device engine, with no reader code involved: inside an
// ordinary app process (beside ART) dlopen the bundled libjvm.so, JNI_CreateJavaVM, boot
// the Suwayomi jar, ask { aboutServer { version } } over loopback. No fork, no exec.
// Results go to logcat under the tag `jvmprobe`. Built and driven by run.sh.
#include <arpa/inet.h>
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <jni.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include <android/log.h>
#define LOG(...) __android_log_print(ANDROID_LOG_INFO, "jvmprobe", __VA_ARGS__)

static double t0_ms;
static int g_port;

static double now_ms(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

static void log_mem(const char *when) {
  FILE *f = fopen("/proc/self/status", "r");
  if (!f) return;
  char line[256], rss[64] = "", hwm[64] = "", thr[64] = "";
  while (fgets(line, sizeof line, f)) {
    line[strcspn(line, "\n")] = 0;
    if (!strncmp(line, "VmRSS:", 6)) snprintf(rss, sizeof rss, "%s", line + 6);
    if (!strncmp(line, "VmHWM:", 6)) snprintf(hwm, sizeof hwm, "%s", line + 6);
    if (!strncmp(line, "Threads:", 8)) snprintf(thr, sizeof thr, "%s", line + 8);
  }
  fclose(f);
  LOG("MEM %s: VmRSS=%s VmHWM=%s Threads=%s", when, rss, hwm, thr);
}

// POST { aboutServer { version } }; copies the response body tail into out. 0 on a version.
static int about_server(char *out, size_t n) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) return -1;
  struct sockaddr_in a = {0};
  a.sin_family = AF_INET;
  a.sin_port = htons(g_port);
  inet_pton(AF_INET, "127.0.0.1", &a.sin_addr);
  if (connect(fd, (struct sockaddr *)&a, sizeof a) != 0) {
    close(fd);
    return -1;
  }
  const char *body = "{\"query\":\"{ aboutServer { version } }\"}";
  char req[512];
  int len = snprintf(req, sizeof req,
                     "POST /api/graphql HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n"
                     "Content-Length: %zu\r\nConnection: close\r\n\r\n%s",
                     strlen(body), body);
  if (write(fd, req, len) != len) {
    close(fd);
    return -1;
  }
  char buf[4096];
  size_t got = 0;
  ssize_t r;
  while (got < sizeof buf - 1 && (r = read(fd, buf + got, sizeof buf - 1 - got)) > 0) got += r;
  close(fd);
  buf[got] = 0;
  char *v = strstr(buf, "\"version\"");
  if (!v) return -1;
  snprintf(out, n, "%s", v);
  return 0;
}

static void *poller(void *arg) {
  (void)arg;
  char out[256];
  for (;;) {
    if (about_server(out, sizeof out) == 0) {
      LOG("READY cold_start_ms=%.0f response=%s", now_ms() - t0_ms, out);
      log_mem("at ready");
      sleep(20);
      log_mem("ready+20s");
      return NULL;
    }
    if (now_ms() - t0_ms > 300000) {
      LOG("TIMEOUT no answer in 300 s");
      return NULL;
    }
    usleep(100 * 1000);
  }
}

typedef jint (*create_vm_fn)(JavaVM **, void **, void *);

// libdir: the app's nativeLibraryDir. home: java.home (holds lib/modules). jar: the class
// path. data: the engine's rootDir. Returns after the engine answered or failed; the VM
// keeps running.
int probe_run(const char *libdir, const char *home, const char *jar, const char *data, int port) {
  t0_ms = now_ms();
  g_port = port;
  char p[1024], q[1024];

  mkdir(data, 0700);
  snprintf(p, sizeof p, "%s/tmp", data);
  mkdir(p, 0700);
  snprintf(p, sizeof p, "%s/server.conf", data);
  FILE *f = fopen(p, "w");
  if (!f) {
    LOG("FAIL write %s: %s", p, strerror(errno));
    return 1;
  }
  fprintf(f,
          "server.ip = \"127.0.0.1\"\nserver.port = %d\nserver.webUIEnabled = false\n"
          "server.initialOpenInBrowserEnabled = false\nserver.systemTrayEnabled = false\n"
          "server.kcefEnabled = false\nserver.flareSolverrEnabled = false\n"
          "server.backupInterval = 0\nserver.globalUpdateInterval = 0\n",
          port);
  fclose(f);

  // scripts/android-jvm-shim.c (packed as libandroid-spawn.so) tells HotSpot that
  // libjvm.so is at <home>/lib/server/libjvm.so.
  snprintf(p, sizeof p, "%s/libandroid-spawn.so", libdir);
  void *shim = dlopen(p, RTLD_NOW);
  void (*set_home)(const char *) = shim ? (void (*)(const char *))dlsym(shim, "komiq_jvm_set_home") : NULL;
  if (!set_home) {
    LOG("FAIL komiq_jvm_set_home in %s: %s", p, dlerror());
    return 2;
  }
  set_home(home);

  // HotSpot loads its own libraries from <java.home>/lib, some of them before it reads
  // any option. Link each bundled library there. The links point into the (read-only)
  // library dir, whose path changes with every install, so they are made on every start.
  DIR *d = opendir(libdir);
  struct dirent *e;
  int links = 0;
  while (d && (e = readdir(d))) {
    size_t l = strlen(e->d_name);
    if (l < 7 || strncmp(e->d_name, "lib", 3) || strcmp(e->d_name + l - 3, ".so")) continue;
    snprintf(p, sizeof p, "%s/%s", libdir, e->d_name);
    snprintf(q, sizeof q, "%s/lib/%s", home, e->d_name);
    unlink(q);
    if (symlink(p, q) == 0) links++;
    else LOG("FAIL symlink %s: %s", q, strerror(errno));
  }
  if (d) closedir(d);
  LOG("OK linked %d libraries into %s/lib", links, home);

  snprintf(p, sizeof p, "%s/libjvm.so", libdir);
  void *h = dlopen(p, RTLD_NOW | RTLD_LOCAL);
  if (!h) {
    LOG("FAIL dlopen %s: %s", p, dlerror());
    return 2;
  }
  LOG("OK dlopen libjvm.so in %.0f ms", now_ms() - t0_ms);
  create_vm_fn create = (create_vm_fn)dlsym(h, "JNI_CreateJavaVM");
  if (!create) {
    LOG("FAIL dlsym JNI_CreateJavaVM: %s", dlerror());
    return 3;
  }

  char o[16][1200];
  int n = 0;
  snprintf(o[n++], 1200, "-Djava.library.path=%s", libdir);
  snprintf(o[n++], 1200, "-Djava.class.path=%s", jar);
  snprintf(o[n++], 1200, "-Dsuwayomi.tachidesk.config.server.rootDir=%s", data);
  snprintf(o[n++], 1200, "-Djava.io.tmpdir=%s/tmp", data);
  snprintf(o[n++], 1200, "-Duser.home=%s", data);
  snprintf(o[n++], 1200, "-Djava.awt.headless=true");
  snprintf(o[n++], 1200, "-Xmx256m");
  snprintf(o[n++], 1200, "-XX:+UseSerialGC");
  snprintf(o[n++], 1200, "-XX:-UsePerfData");
  snprintf(o[n++], 1200, "-Xrs");
  JavaVMOption opts[16];
  for (int i = 0; i < n; i++) {
    opts[i].optionString = o[i];
    opts[i].extraInfo = NULL;
  }
  JavaVMInitArgs args = {0x00010008, n, opts, JNI_FALSE};

  JavaVM *vm = NULL;
  JNIEnv *env = NULL;
  double t1 = now_ms();
  jint rc = create(&vm, (void **)&env, &args);
  if (rc != JNI_OK) {
    LOG("FAIL JNI_CreateJavaVM rc=%d", rc);
    return 4;
  }
  LOG("OK JNI_CreateJavaVM in %.0f ms", now_ms() - t1);
  log_mem("after CreateJavaVM");

  jclass sys = (*env)->FindClass(env, "java/lang/System");
  jmethodID gp = (*env)->GetStaticMethodID(env, sys, "getProperty", "(Ljava/lang/String;)Ljava/lang/String;");
  const char *keys[] = {"java.home", "java.vm.name", "java.vm.info", "java.runtime.version", "os.arch", "sun.boot.library.path"};
  for (int i = 0; i < 6; i++) {
    jstring k = (*env)->NewStringUTF(env, keys[i]);
    jstring v = (jstring)(*env)->CallStaticObjectMethod(env, sys, gp, k);
    const char *c = v ? (*env)->GetStringUTFChars(env, v, NULL) : "(null)";
    LOG("PROP %s=%s", keys[i], c);
  }

  pthread_t th;
  pthread_create(&th, NULL, poller, NULL);

  jclass cls = (*env)->FindClass(env, "suwayomi/tachidesk/MainKt");
  if (!cls) {
    (*env)->ExceptionDescribe(env);
    LOG("FAIL FindClass suwayomi/tachidesk/MainKt");
    return 5;
  }
  jmethodID mid = (*env)->GetStaticMethodID(env, cls, "main", "([Ljava/lang/String;)V");
  jobjectArray argv = (*env)->NewObjectArray(env, 0, (*env)->FindClass(env, "java/lang/String"), NULL);
  (*env)->CallStaticVoidMethod(env, cls, mid, argv);
  if ((*env)->ExceptionCheck(env)) {
    (*env)->ExceptionDescribe(env);
    LOG("FAIL Suwayomi main threw");
    return 6;
  }
  LOG("OK main returned after %.0f ms", now_ms() - t0_ms);
  pthread_join(th, NULL);
  return 0;
}

static char a_lib[1024], a_home[1024], a_jar[1024], a_data[1024];
static int a_port;
static void *vm_thread(void *x) {
  (void)x;
  int rc = probe_run(a_lib, a_home, a_jar, a_data, a_port);
  LOG("EXIT rc=%d", rc);
  return NULL;
}
JNIEXPORT jint JNICALL Java_app_komika_jvmprobe_MainActivity_run(JNIEnv *env, jclass c, jstring libdir, jstring home,
                                                                 jstring jar, jstring data, jint port) {
  (void)c;
  snprintf(a_lib, sizeof a_lib, "%s", (*env)->GetStringUTFChars(env, libdir, NULL));
  snprintf(a_home, sizeof a_home, "%s", (*env)->GetStringUTFChars(env, home, NULL));
  snprintf(a_jar, sizeof a_jar, "%s", (*env)->GetStringUTFChars(env, jar, NULL));
  snprintf(a_data, sizeof a_data, "%s", (*env)->GetStringUTFChars(env, data, NULL));
  a_port = port;
  // An app's stdout and stderr go nowhere; keep what the VM and the engine print.
  char out[1100];
  snprintf(out, sizeof out, "%s.stdout.log", a_data);
  freopen(out, "w", stdout);
  freopen(out, "a", stderr);
  setvbuf(stdout, NULL, _IOLBF, 0);
  pthread_attr_t at;
  pthread_attr_init(&at);
  pthread_attr_setstacksize(&at, 16 * 1024 * 1024);
  pthread_t th;
  return pthread_create(&th, &at, vm_thread, NULL);
}
