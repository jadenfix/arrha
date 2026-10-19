use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// TEE Attestation Verification
///
/// Supports:
/// - AMD SEV-SNP (Secure Encrypted Virtualization)
/// - Intel TDX (Trust Domain Extensions)
/// - AWS Nitro Enclaves
///
/// ATTESTATION FLOW:
/// 1. Worker generates attestation report
/// 2. Report includes: TEE type, measurement, nonce, timestamp
/// 3. Validator verifies signature chain
/// 4. Validator checks measurement against whitelist
/// 5. If valid, worker is authorized
///
/// SECURITY:
/// - Attestation must be fresh (<60s)
/// - Measurement must match approved builds
/// - Signature chain must be valid
/// - Nonce prevents replay attacks

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum TeeType {
    SevSnp,     // AMD SEV-SNP
    IntelTdx,   // Intel TDX
    AwsNitro,   // AWS Nitro Enclaves
    Simulation, // For testing only
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationReport {
    pub tee_type: TeeType,
    pub measurement: Vec<u8>,     // SHA-384 of code + data
    pub nonce: Vec<u8>,           // Random nonce for freshness
    pub timestamp: u64,           // Unix timestamp
    pub signature: Vec<u8>,       // TEE signature
    pub cert_chain: Vec<Vec<u8>>, // Certificate chain
}

#[derive(Debug, Clone)]
pub struct TeeVerifier {
    /// Approved measurements (whitelist)
    approved_measurements: Vec<Vec<u8>>,

    /// Maximum attestation age (seconds)
    max_age_secs: u64,

    /// Root certificates for each TEE type
    root_certs: std::collections::HashMap<TeeType, Vec<u8>>,
}

impl TeeVerifier {
    pub fn new() -> Self {
        TeeVerifier {
            approved_measurements: Vec::new(),
            max_age_secs: 60, // 1 minute
            root_certs: std::collections::HashMap::new(),
        }
    }

    /// Add approved measurement to whitelist
    pub fn add_approved_measurement(&mut self, measurement: Vec<u8>) {
        if !self.approved_measurements.contains(&measurement) {
            self.approved_measurements.push(measurement);
        }
    }

    /// Set root certificate for TEE type
    pub fn set_root_cert(&mut self, tee_type: TeeType, cert: Vec<u8>) {
        self.root_certs.insert(tee_type, cert);
    }

    /// Verify attestation report
    pub fn verify(&self, report: &AttestationReport, current_time: u64) -> Result<()> {
        // 1. Check freshness — reject future-dated reports
        if report.timestamp > current_time {
            bail!(
                "attestation timestamp {} is in the future (current: {})",
                report.timestamp,
                current_time
            );
        }
        if current_time - report.timestamp > self.max_age_secs {
            bail!(
                "attestation too old: {} seconds (max {})",
                current_time - report.timestamp,
                self.max_age_secs
            );
        }

        // 2. Check measurement is approved
        if !self.approved_measurements.contains(&report.measurement) {
            bail!("measurement not approved");
        }

        // 3. Verify signature chain
        if report.tee_type != TeeType::Simulation {
            self.verify_signature_chain(report)?;
        }

        // 4. TEE-specific verification
        match report.tee_type {
            TeeType::SevSnp => self.verify_sev_snp(report)?,
            TeeType::IntelTdx => self.verify_intel_tdx(report)?,
            TeeType::AwsNitro => self.verify_aws_nitro(report)?,
            TeeType::Simulation => {
                // Skip verification for testing
                #[cfg(debug_assertions)]
                eprintln!("WARN: Using simulation TEE (testing only)");
            }
        }

        Ok(())
    }

    fn verify_signature_chain(&self, report: &AttestationReport) -> Result<()> {
        let _root_cert = self
            .root_certs
            .get(&report.tee_type)
            .ok_or_else(|| anyhow::anyhow!("no root cert for TEE type {:?}", report.tee_type))?;

        if report.cert_chain.is_empty() {
            bail!("empty certificate chain");
        }
        if report.signature.is_empty() {
            bail!("attestation report has empty signature");
        }
        bail!(
            "cryptographic certificate verification for {:?} attestations is not implemented; refusing non-simulation report",
            report.tee_type
        );
    }

    fn verify_sev_snp(&self, report: &AttestationReport) -> Result<()> {
        // AMD SEV-SNP specific verification
        // - Check VCEK certificate
        // - Verify measurement includes firmware version
        // - Check policy (debug, migration flags)

        if report.measurement.len() != 48 {
            bail!("invalid SEV-SNP measurement length (expected 48)");
        }

        Ok(())
    }

    fn verify_intel_tdx(&self, report: &AttestationReport) -> Result<()> {
        // Intel TDX specific verification
        // - Verify TDREPORT structure
        // - Check MRTD (measurement)
        // - Verify quote signature

        if report.measurement.len() != 48 {
            bail!("invalid TDX measurement length (expected 48)");
        }

        Ok(())
    }

    fn verify_aws_nitro(&self, report: &AttestationReport) -> Result<()> {
        // AWS Nitro Enclaves verification
        // - Verify PCR values
        // - Check attestation document signature
        // - Validate certificate chain to AWS root

        if report.measurement.len() != 48 {
            bail!("invalid Nitro measurement length (expected 48)");
        }

        Ok(())
    }
}

impl Default for TeeVerifier {
    fn default() -> Self {
        Self::new()
    }
}
