// Generate the fixture against the existing canonical chiavdf source, not this Rust verifier.
// c++ -std=c++17 -O2 -I<chiavdf>/src -I<gmp-include> generate.cpp <lzcnt-object> -L<gmp-lib> -lgmp -lgmpxx -pthread -o generate
// ./generate checkpointed-proof.bin
#include "verifier.h"
#include "prover_slow.h"
#include <fstream>
static void checkpoint(std::vector<uint8_t>& blob, integer& D, form x, const std::vector<uint8_t>& proof, uint64_t n) {
    form y=DeserializeForm(D,proof.data(),100);
    auto prime=GetB(D,x,y).to_bytes();
    uint8_t iters[8]; Int64ToBytes(iters,n); blob.insert(blob.end(),iters,iters+8);
    blob.insert(blob.end(),33-prime.size(),0); blob.insert(blob.end(),prime.begin(),prime.end());
    blob.insert(blob.end(),proof.begin()+100,proof.end());
}
int main(int argc, char** argv) {
    if(argc != 2) return 2;
    std::vector<uint8_t> seed(32,42), initial(100,0); initial[0]=8;
    integer D=CreateDiscriminant(seed,1024); form x=DeserializeForm(D,initial.data(),100);
    auto first=ProveSlow(D,x,80,""); form mid=DeserializeForm(D,first.data(),100);
    auto second=ProveSlow(D,mid,100,""); form last=DeserializeForm(D,second.data(),100);
    auto blob=ProveSlow(D,last,180,"");
    checkpoint(blob,D,mid,second,100); checkpoint(blob,D,x,first,80);
    if(!CheckProofOfTimeNWesolowski(D,initial.data(),blob.data(),blob.size(),360,1024,2)) return 3;
    std::ofstream file(argv[1],std::ios::binary); file.write((char*)blob.data(),blob.size()); return 0;
}
