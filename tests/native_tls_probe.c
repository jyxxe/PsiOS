#include <stdio.h>
#include <openssl/pem.h>
#include <openssl/x509_vfy.h>
#include <openssl/x509v3.h>

int main(int argc, char **argv)
{
    int result = 1;
    X509 *leaf = NULL;
    X509_STORE_CTX *verification = NULL;
    X509_STORE *store = X509_STORE_new();
    if (!store || !X509_STORE_load_locations(store, "/etc/ssl/certs/ca-certificates.crt", NULL)) {
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
