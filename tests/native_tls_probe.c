#define _POSIX_C_SOURCE 200809L
#include <arpa/inet.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>
#include <openssl/pem.h>
#include <openssl/ssl.h>
#include <openssl/x509_vfy.h>
#include <openssl/x509v3.h>

static const char store_path[] = "/etc/ssl/certs/ca-certificates.crt";
static const char request[] = "GET / HTTP/1.0\r\n\r\n";
static const char reply[] = "HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nPsiOS";

static int serve(int listener, SSL_CTX *context)
{
    char received[sizeof(request) - 1];
    int result = 1;
    int peer = accept(listener, NULL, NULL);
    SSL *server = SSL_new(context);
    close(listener);
    if (peer < 0 || !server || !SSL_set_fd(server, peer))
        goto done;
    if (SSL_accept(server) != 1) {
        result = 2;
        goto done;
    }
    if (SSL_read(server, received, sizeof(received)) == (int)sizeof(received)
        && memcmp(received, request, sizeof(received)) == 0
        && SSL_write(server, reply, sizeof(reply) - 1) == (int)sizeof(reply) - 1) {
        SSL_shutdown(server);
        result = 0;
    }
done:
    SSL_free(server);
    if (peer >= 0)
        close(peer);
    return result;
}

static int exchange(int peer, SSL_CTX *context, const char *hostname)
{
    char received[sizeof(reply) - 1];
    int result = 1;
    SSL *client = SSL_new(context);
    if (!client || !SSL_set_fd(client, peer) || !SSL_set1_host(client, hostname))
        goto done;
    if (SSL_connect(client) != 1) {
        long error = SSL_get_verify_result(client);
        if (error != X509_V_OK) {
            puts(X509_verify_cert_error_string(error));
            result = 2;
        }
        goto done;
    }
    if (SSL_version(client) == TLS1_3_VERSION
        && SSL_write(client, request, sizeof(request) - 1) == (int)sizeof(request) - 1
        && SSL_read(client, received, sizeof(received)) == (int)sizeof(received)
        && memcmp(received, reply, sizeof(received)) == 0) {
        SSL_shutdown(client);
        puts("verified TLS 1.3 and HTTP response");
        result = 0;
    }
done:
    SSL_free(client);
    return result;
}

static int handshake(const char *certificate, const char *key, const char *hostname)
{
    int result = 1;
    int listener = -1;
    SSL_CTX *server = SSL_CTX_new(TLS_server_method());
    SSL_CTX *client = SSL_CTX_new(TLS_client_method());
    if (!server || !client
        || !SSL_CTX_set_min_proto_version(server, TLS1_3_VERSION)
        || !SSL_CTX_set_min_proto_version(client, TLS1_3_VERSION)
        || SSL_CTX_use_certificate_chain_file(server, certificate) != 1
        || SSL_CTX_use_PrivateKey_file(server, key, SSL_FILETYPE_PEM) != 1
        || SSL_CTX_load_verify_locations(client, store_path, NULL) != 1)
        goto done;
    SSL_CTX_set_verify(client, SSL_VERIFY_PEER, NULL);
    signal(SIGPIPE, SIG_IGN);
    alarm(20);
    struct sockaddr_in address = { .sin_family = AF_INET, .sin_addr = { .s_addr = htonl(INADDR_LOOPBACK) } };
    socklen_t length = sizeof(address);
    listener = socket(AF_INET, SOCK_STREAM, 0);
    if (listener < 0 || bind(listener, (struct sockaddr *)&address, length) < 0
        || getsockname(listener, (struct sockaddr *)&address, &length) < 0
        || listen(listener, 1) < 0)
        goto done;
    pid_t child = fork();
    if (child < 0)
        goto done;
    if (child == 0) {
        alarm(20);
        _exit(serve(listener, server));
    }
    close(listener);
    listener = -1;
    int peer = socket(AF_INET, SOCK_STREAM, 0);
    if (peer >= 0) {
        if (connect(peer, (struct sockaddr *)&address, length) == 0)
            result = exchange(peer, client, hostname);
        close(peer);
    }
    if (result == 1)
        kill(child, SIGTERM);
    int status;
    if (waitpid(child, &status, 0) < 0 || !WIFEXITED(status) || WEXITSTATUS(status) != result)
        result = 1;
done:
    if (listener >= 0)
        close(listener);
    SSL_CTX_free(client);
    SSL_CTX_free(server);
    return result;
}

int main(int argc, char **argv)
{
    if (argc == 5 && strcmp(argv[1], "--handshake") == 0)
        return handshake(argv[2], argv[3], argv[4]);
    int result = 1;
    X509 *leaf = NULL;
    X509_STORE_CTX *verification = NULL;
    X509_STORE *store = X509_STORE_new();
    if (!store || !X509_STORE_load_locations(store, store_path, NULL)) {
        fputs("could not load the native TLS certificate store\n", stderr);
        goto done;
    }
    if (argc == 1) {
        STACK_OF(X509) *roots = X509_STORE_get1_all_certs(store);
        if (!roots)
            goto done;
        printf("loaded %d TLS roots\n", sk_X509_num(roots));
        sk_X509_pop_free(roots, X509_free);
        result = 0;
        goto done;
    }
    if (argc != 3)
        goto done;
    FILE *pem = fopen(argv[1], "r");
    if (!pem)
        goto done;
    leaf = PEM_read_X509(pem, NULL, NULL, NULL);
    fclose(pem);
    verification = X509_STORE_CTX_new();
    if (!leaf || !verification || !X509_STORE_CTX_init(verification, store, leaf, NULL) ||
        !X509_STORE_CTX_set_purpose(verification, X509_PURPOSE_SSL_SERVER) ||
        !X509_VERIFY_PARAM_set1_host(X509_STORE_CTX_get0_param(verification), argv[2], 0))
        goto done;
    if (X509_verify_cert(verification) == 1) {
        puts("verified HTTPS certificate");
        result = 0;
    } else {
        puts(X509_verify_cert_error_string(X509_STORE_CTX_get_error(verification)));
        result = 2;
    }

done:
    X509_STORE_CTX_free(verification);
    X509_free(leaf);
    X509_STORE_free(store);
    return result;
}
