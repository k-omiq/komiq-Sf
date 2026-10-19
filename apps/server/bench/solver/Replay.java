// Replay a solved page from the JVM's own TLS stack, which is what Suwayomi's
// OkHttp uses on a server. Prints "status wall" (wall: none|challenge).
// Usage: java Replay.java URL USER_AGENT COOKIE_HEADER
import java.net.InetSocketAddress;
import java.net.ProxySelector;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.time.Duration;

public class Replay {
  public static void main(String[] a) throws Exception {
    HttpClient.Builder b = HttpClient.newBuilder()
        .followRedirects(HttpClient.Redirect.NORMAL)
        .connectTimeout(Duration.ofSeconds(15));
    // HttpClient has no SOCKS support; WARP's proxy port also speaks HTTP CONNECT.
    String ph = System.getenv("PROXY_HOST");
    if (ph != null && !ph.isEmpty()) {
      b.proxy(ProxySelector.of(new InetSocketAddress(ph, Integer.parseInt(System.getenv("PROXY_PORT")))));
    }
    HttpClient client = b.build();
    HttpRequest.Builder req = HttpRequest.newBuilder(URI.create(a[0]))
        .timeout(Duration.ofSeconds(20))
        .header("User-Agent", a[1])
        .header("Accept", "text/html")
        .header("Accept-Language", "en-US,en;q=0.9");
    if (a.length > 2 && !a[2].isEmpty()) req.header("Cookie", a[2]);
    try {
      HttpResponse<String> r = client.send(req.build(), HttpResponse.BodyHandlers.ofString());
      String body = r.body();
      boolean wall = r.headers().firstValue("cf-mitigated").isPresent()
          || body.contains("challenges.cloudflare.com") || body.contains("_cf_chl_opt");
      System.out.println(r.statusCode() + " " + (wall ? "challenge" : "none"));
    } catch (Exception e) {
      System.out.println("000 error");
    }
  }
}
