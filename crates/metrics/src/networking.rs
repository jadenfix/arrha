// Networking Metrics
use once_cell::sync::Lazy;
use prometheus::{
    register_histogram, register_int_counter, register_int_gauge, Histogram, IntCounter, IntGauge,
};

pub struct NetworkingMetrics {
    // Connection metrics
    pub connections_total: IntCounter,
    pub connections_active: IntGauge,
    pub connection_errors: IntCounter,
    pub connection_duration_seconds: Histogram,

    // QUIC transport metrics
    pub quic_bytes_sent: IntCounter,
    pub quic_bytes_received: IntCounter,
    pub quic_rtt_ms: Histogram,
    pub quic_streams_opened: IntCounter,
    pub quic_streams_closed: IntCounter,

    // Message metrics
    pub messages_sent: IntCounter,
    pub messages_received: IntCounter,
    pub message_latency_ms: Histogram,
    pub message_size_bytes: Histogram,

    // Peer metrics
    pub peers_total: IntGauge,
    pub peers_connected: IntGauge,
    pub peer_reputation_score: Histogram,

    // Bandwidth metrics
    pub bandwidth_in_mbps: Histogram,
    pub bandwidth_out_mbps: Histogram,
}

impl NetworkingMetrics {
    fn new() -> Self {
        NetworkingMetrics {
            connections_total: register_int_counter!(
                "aether_net_connections_total",
                "Total number of connections established"
            )
            .expect("register connections_total"),

            connections_active: register_int_gauge!(
                "aether_net_connections_active",
                "Number of currently active connections"
            )
            .expect("register connections_active"),

            connection_errors: register_int_counter!(
                "aether_net_connection_errors_total",
                "Total connection errors"
            )
            .expect("register connection_errors"),

            connection_duration_seconds: register_histogram!(
                "aether_net_connection_duration_seconds",
                "Connection duration in seconds",
                vec![1.0, 10.0, 60.0, 300.0, 1800.0, 3600.0]
            )
            .expect("register connection_duration"),

            quic_bytes_sent: register_int_counter!(
                "aether_net_quic_bytes_sent_total",
                "Total bytes sent via QUIC"
            )
            .expect("register quic_bytes_sent"),

            quic_bytes_received: register_int_counter!(
                "aether_net_quic_bytes_received_total",
                "Total bytes received via QUIC"
            )
            .expect("register quic_bytes_received"),

            quic_rtt_ms: register_histogram!(
                "aether_net_quic_rtt_ms",
                "QUIC round-trip time in milliseconds",
                vec![1.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0]
            )
            .expect("register quic_rtt"),

            quic_streams_opened: register_int_counter!(
                "aether_net_quic_streams_opened_total",
                "Total QUIC streams opened"
            )
            .expect("register quic_streams_opened"),

            quic_streams_closed: register_int_counter!(
                "aether_net_quic_streams_closed_total",
                "Total QUIC streams closed"
            )
            .expect("register quic_streams_closed"),

            messages_sent: register_int_counter!(
                "aether_net_messages_sent_total",
                "Total messages sent"
            )
            .expect("register messages_sent"),

            messages_received: register_int_counter!(
                "aether_net_messages_received_total",
                "Total messages received"
            )
            .expect("register messages_received"),

            message_latency_ms: register_histogram!(
                "aether_net_message_latency_ms",
                "Message propagation latency in milliseconds",
                vec![1.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0]
            )
            .expect("register message_latency"),

            message_size_bytes: register_histogram!(
                "aether_net_message_size_bytes",
                "Message size in bytes",
                vec![100.0, 1000.0, 10000.0, 100000.0, 1000000.0]
            )
            .expect("register message_size"),

            peers_total: register_int_gauge!(
                "aether_net_peers_total",
                "Total number of known peers"
            )
            .expect("register peers_total"),

            peers_connected: register_int_gauge!(
                "aether_net_peers_connected",
                "Number of currently connected peers"
            )
            .expect("register peers_connected"),

            peer_reputation_score: register_histogram!(
                "aether_net_peer_reputation_score",
                "Peer reputation scores",
                vec![0.0, 0.2, 0.4, 0.6, 0.8, 1.0]
            )
            .expect("register peer_reputation"),

            bandwidth_in_mbps: register_histogram!(
                "aether_net_bandwidth_in_mbps",
                "Inbound bandwidth in Mbps",
                vec![0.1, 1.0, 10.0, 100.0, 1000.0]
            )
            .expect("register bandwidth_in"),

            bandwidth_out_mbps: register_histogram!(
                "aether_net_bandwidth_out_mbps",
                "Outbound bandwidth in Mbps",
                vec![0.1, 1.0, 10.0, 100.0, 1000.0]
            )
            .expect("register bandwidth_out"),
        }
    }
}

pub static NET_METRICS: Lazy<NetworkingMetrics> = Lazy::new(NetworkingMetrics::new);
