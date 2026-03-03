package common

func Fib(n int64) uint64 {
	if n <= 1 {
		return uint64(n)
	}
	var a, b uint64 = 0, 1
	for i := int64(2); i <= n; i++ {
		a, b = b, a+b
	}
	return b
}

const (
	GatewayHTTPPort = "8080"
	ComputeHTTPPort = "8081"
	StoreHTTPPort   = "8082"
)
