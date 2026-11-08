use sequoia_openpgp as openpgp;
use openpgp::{
    KeyHandle,
    anyhow::anyhow as openpgp_err,
    cert::Cert,
    parse::stream::{
        MessageLayer,
        MessageStructure,
        VerificationHelper,
    },
};


/// Supplies the trusted certs and enforces that at least one signature is good.
pub struct SigningHelper {
    pub certs: Vec<Cert>,
}

impl VerificationHelper for SigningHelper {
    fn get_certs(&mut self, _ids: &[KeyHandle]) -> openpgp::Result<Vec<Cert>> {
        Ok(self.certs.clone())
    }

    fn check(&mut self, structure: MessageStructure) -> openpgp::Result<()> {
        for layer in structure {
            if let MessageLayer::SignatureGroup { results } = layer {
                if results.iter().any(|r| r.is_ok()) {
                    return Ok(());
                }

                let reasons: Vec<String> = results
                    .iter()
                    .filter_map(|r| r.as_ref().err().map(|e| e.to_string()))
                    .collect();

                return Err(openpgp_err!(
                    "no valid signature from a trusted key: {}",
                    reasons.join("; ")
                ));
            }
        }
        Err(openpgp_err!("message contained no signature"))
    }
}

